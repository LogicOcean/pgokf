# pgokf troubleshooting

`pgokf` reports failures as stable PostgreSQL **SQLSTATEs**, so clients can react
programmatically instead of matching message text. Errors tied to a specific
source file also carry the offending bundle-relative path in the form
`[bundle-relative path: …]`; validation, configuration, and limit errors that
have no file context render as the bare message with no path suffix.

See the exact SQLSTATE of the last error with `\errverbose` in `psql`, or run the
session with `\set VERBOSITY verbose` to have the code printed inline.

## SQLSTATE map

| SQLSTATE | Class | Raised by |
| -------- | ----- | --------- |
| `22023` | invalid parameter value | bad path, malformed bundle content, out-of-range argument, invalid/unknown configuration, unknown/ambiguous identifier, invalid embedding, a required optional extension (pgvector, pg_cron) that is not installed |
| `42501` | insufficient privilege | missing role membership or `EXECUTE` grant |
| `23505` | unique violation | registering an already-registered bundle path |
| `XX000` | internal error | a broken installation invariant (should not occur in normal use) |

All error strings below were produced against a live cluster.

---

## `42501` - permission denied

### `permission denied for function register_bundle` (or another function)

```text
ERROR:  42501: permission denied for function register_bundle
ERROR:  42501: permission denied for function concept_search
```

**Cause.** The current login user is not a member of the role tier that owns
the operation. The three tiers are `pgokf_reader` < `pgokf_writer` <
`pgokf_admin`, each inheriting the one below:

- **`pgokf_writer`** owns ingestion and the bundle lifecycle:
  `register_bundle`, `register_bundle_content`, `refresh_bundle`,
  `unregister_bundle`, `set_bundle_enabled`, `retire_bundle`,
  `unretire_bundle`, and `set_concept_embedding`.
- **`pgokf_admin`** owns configuration, the file-writing exports, and
  maintenance: `set_config`, `reset_config`, `export_parquet`,
  `export_sources`, `purge_retired`, `rebuild_search_index`,
  `rebuild_embedding_index`, `schedule_refresh`, `unschedule_refresh`, and
  `list_access_log`.
- **`pgokf_reader`** owns every read path: `concept_search` and the other
  search/similarity/facet functions, `concept_neighbors`, `concept_history`,
  `concept_as_of`, `list_bundles`, `bundle_info`, `list_sync_log`,
  `list_sync_changes`, `list_bundle_log`, `catalog_stats`, `health`,
  `get_config`, and `get_concept_source`.

**Fix.** Grant the appropriate role tier to the user:

```sql
GRANT pgokf_reader TO analytics_ro;   -- read + search
GRANT pgokf_writer TO ingest_bot;     -- register/refresh bundles (inherits reader)
GRANT pgokf_admin  TO catalog_ops;    -- configure + export (inherits writer)
```

Confirm membership with `\du` or:

```sql
SELECT pg_has_role('analytics_ro', 'pgokf_reader', 'MEMBER');
```

### `permission denied for schema pgokf`

**Cause.** The user has no `USAGE` on the schema (they were not granted any of
the three roles). **Fix.** Grant `pgokf_reader`, `pgokf_writer`, or
`pgokf_admin` as above; all three carry schema `USAGE`.

### Export destination directory is not writable

```text
ERROR:  42501: destination directory is not writable: /srv/exports/readonly
```

**Cause.** `export_parquet` and `export_sources` probe the destination with an
`O_NOFOLLOW` write before exporting; if the PostgreSQL server's OS user cannot
write there, the directory exists but the write is refused with `42501` (a
privilege condition), not `22023`. **Fix.** Choose a directory the server's OS
account owns or has write permission on, and (if `allowed_roots` is configured)
one contained within an allowed root.

---

## `22023` - invalid parameter

### Path is not absolute

```text
ERROR:  22023: path must be absolute: relative/path
```

**Cause.** `register_bundle` requires an absolute path, resolved by the **server**
(not the client). **Fix.** Pass a full absolute path the PostgreSQL server
process can reach, e.g. `/srv/okf-bundles/handbook`.

### Path traversal / NUL byte

```text
ERROR:  22023: path traversal is not allowed: /srv/bundles/../secrets
```

**Cause.** The path contains a `..` component (rejected before canonicalization)
or a NUL byte. **Fix.** Supply a clean absolute path with no parent-directory
components.

### Path outside `allowed_roots`

```text
ERROR:  22023: resolved path /tmp/.../outside-bundle is outside allowed_roots
```

**Cause.** `allowed_roots` is configured and the bundle path resolves outside
every configured root (containment is checked with both sides canonicalized, so a
symlink cannot escape). **Fix.** Either place the bundle under an allowed root, or
add its root:

```sql
SELECT pgokf.get_config() -> 'allowed_roots';                      -- inspect
SELECT pgokf.set_config('allowed_roots', '["/srv/okf-bundles"]'::jsonb);
```

### Directory does not exist / is not a directory

```text
ERROR:  22023: failed to canonicalize bundle path /no/such/dir/here: No such file or directory (os error 2)
ERROR:  22023: bundle path is not a directory: /srv/okf-bundles/handbook.md
```

**Cause.** The path cannot be canonicalized (missing, or a permission problem for
the server's OS user) or resolves to a non-directory. **Fix.** Verify the
directory exists and is readable by the PostgreSQL server's OS account.

### Malformed frontmatter (strict parse failure)

```text
ERROR:  22023: failed to parse OKF concept: broken.md: Markdown file must begin with a YAML frontmatter delimiter (`---`)
ERROR:  22023: failed to parse OKF concept: notype.md: invalid YAML frontmatter: missing field `type`
```

**Cause.** Under the default strict policy (`default_strict = true`) the
**first** malformed concept file aborts the whole sync and the transaction rolls
back, so a partial projection is never committed. (With `default_strict` set to
`false`, a malformed file is instead logged as a warning and skipped, and the
rest of the bundle registers.) Common causes: a file that does not begin with a
`---` frontmatter delimiter, unterminated frontmatter, invalid YAML, or a
missing **required** field (`type` and `title` are required). The offending file
appears in the `[bundle-relative path: …]` suffix.

**Fix.** Correct the named file (add a valid `---` YAML block with `type` and
`title`), then re-run `register_bundle` / `refresh_bundle`. Note that
`index.md` and `log.md` are reserved and are never parsed as concepts
(`log.md` is projected into `pgokf.bundle_log` instead), so they cannot cause
this error.

### Bad `limit_count` or `max_hops`

```text
ERROR:  22023: limit_count must be between 1 and 500, got 0
ERROR:  22023: max_hops must be at least 1, got 0
ERROR:  22023: query must not be empty
```

**Cause.** `concept_search(limit_count)` must be in `1..=500`;
`concept_neighbors(max_hops)` must be `>= 1` (and is capped at
`pgokf.max_graph_hops`); a search `query` must contain a non-whitespace
character. **Fix.** Pass in-range arguments.

### Ambiguous concept for `concept_neighbors`

```text
ERROR:  22023: concept_id '…' exists in N bundles; pass bundle_id to disambiguate
```

**Cause.** The concept ID exists in more than one **active** bundle and no
`bundle_id` was given (a disabled or retired duplicate does not count toward
the ambiguity). **Fix.** Pass the third argument:
`SELECT * FROM pgokf.concept_neighbors('runbooks/failover', 2, 1);`.

### Unknown bundle

```text
ERROR:  22023: bundle 999 is not registered
```

**Cause.** A bundle-addressed function (`refresh_bundle`, `unregister_bundle`,
`bundle_info`, `retire_bundle`, `schedule_refresh`, and the rest) was given a
`bundle_id` that does not exist. When multi-tenancy is in use, the same message
also covers a **cross-tenant** id: a session that has set `pgokf.tenant` and
targets a bundle owned by another tenant is answered exactly as if the bundle
were unregistered, so a guessed id cannot probe another tenant's catalog.
**Fix.** Look up the real ID with `SELECT id, path FROM pgokf.list_bundles();`
(under the right `pgokf.tenant` setting). Retired bundles are hidden from
`list_bundles` but remain reachable by id via `bundle_info` and visible in
`catalog_stats`.

### Content-sourced bundle passed to `refresh_bundle`

```text
ERROR:  22023: bundle 3 is content-sourced; content bundles are re-synced by calling pgokf.register_bundle_content, not pgokf.refresh_bundle
```

**Cause.** The bundle was registered with `register_bundle_content`
(`pgokf.bundles.source_type = 'content'`), so it has no filesystem root the
server could re-walk; its bytes only ever arrive from the caller. **Fix.**
Re-sync it by calling `pgokf.register_bundle_content` again with the current
content (the server diffs the streamed set against the stored projection, so
changed concepts are upserted and missing ones removed). This is what the
`pgokf-ingest` companion, including its `--watch` mode, does on every pass.

### Semantic search without pgvector

```text
ERROR:  22023: semantic search requires the pgvector extension, which is not installed; run CREATE EXTENSION vector (or use pgokf.concept_search for lexical search)
```

**Cause.** `concept_search_semantic` ranks by pgvector cosine distance, so it
needs the optional pgvector extension at query time; there is no lexical
fallback that could honestly stand in for a vector ranking. **Fix.** Install
pgvector (`CREATE EXTENSION vector;`), or use the lexical `concept_search`.
The neighboring paths degrade instead of erroring: `concept_search_hybrid`
falls back to lexical-only with a `WARNING`, and `rebuild_embedding_index` is a
logged no-op, when pgvector is absent.

### Scheduled refresh without pg_cron

```text
ERROR:  22023: scheduled refresh requires the pg_cron extension, which is not installed; add pg_cron to shared_preload_libraries and run CREATE EXTENSION pg_cron, or refresh manually with pgokf.refresh_bundle
```

**Cause.** `schedule_refresh` registers a `pg_cron` job, and the optional
`pg_cron` extension is not installed (full scheduling also requires `pg_cron`
in `shared_preload_libraries`). `unschedule_refresh` without `pg_cron` is a
clean no-op returning `false`, not an error. **Fix.** Install and preload
`pg_cron`, or drive `pgokf.refresh_bundle` from an external scheduler (see
[Operations](operations.md)).

### Embedding rejected: wrong length or non-finite element

```text
ERROR:  22023: embedding has 384 dimensions but the configured embedding_dim is 1536; set embedding_dim to match your model or supply a 1536-dimensional vector
ERROR:  22023: embedding element at index 7 is not finite (NaN); every element must be a finite real number ...
```

**Cause.** `set_concept_embedding` validates every vector **before** the
upsert: its length must equal the durable `embedding_dim` configuration key,
and every element must be finite. `NaN` and `Infinity` are rejected at write
time because storage is `real[]` (which would accept them silently) while
pgvector refuses them at every later query and index cast, so one poisoned row
could break semantic and hybrid search, and `rebuild_embedding_index`,
catalog-wide until it was found. The same length check applies to the
`query_embedding` argument of `concept_search_semantic` /
`concept_search_hybrid`. **Fix.** Set `embedding_dim` to your model's output
size before embedding (or supply a vector of the configured length), and drop
non-finite values at the producer. The shipped `pgokf-embed` companion reads
`embedding_dim` from `pgokf.get_config()` automatically.

### Invalid configuration

```text
ERROR:  22023: unknown configuration key: nope
ERROR:  22023: path must be absolute: relative/dir
ERROR:  22023: sync_log_retention_days must be greater than or equal to 0
ERROR:  22023: text search configuration does not exist: no_such_config
```

**Cause.** `set_config` received an unknown key, or a value of the wrong shape or
outside the key's domain. **Fix.** See [configuration.md](configuration.md) for
each key's expected `jsonb` shape and constraints.

### `get_concept_source`: no source stored vs. no such concept

```text
ERROR:  22023: no source is stored for concept runbooks/failover in bundle 1; the bundle was synced with store_source disabled
ERROR:  22023: no such concept runbooks/typo in bundle 1
```

**Cause.** `get_concept_source(bundle_id, concept_id)` raises `22023` in two
**distinct** situations, and the message tells them apart:

- **The concept exists but no source bytes were stored.** Verbatim source
  storage is opt-in: `store_source` was `false` when the bundle was synced, so
  only metadata and search were projected - there is nothing to return. This is
  the deployment tier where originals live in a data lake / mounted bucket.
- **No such concept.** The `(bundle_id, concept_id)` pair does not exist at all
  (wrong id, wrong bundle, or the concept was removed on a later refresh).

**Fix.** For the first case, enable source storage and re-register (the setting
is **not retroactive** - see below), or read the original from wherever the data
lake keeps it. For the second, look up the real id and bundle:

```sql
SELECT bundle_id, id FROM pgokf.concepts WHERE id = 'runbooks/failover';
```

### `store_source` is not retroactive

Enabling `store_source` after a bundle is already synced does **not** backfill
the stored bytes - like `default_text_search_config`, it is read at sync time.
A concept synced while `store_source` was `false` has no `pgokf.concept_source`
row, so `get_concept_source` / `export_sources` cannot return it.

```sql
SELECT pgokf.set_config('store_source', 'true'::jsonb);  -- set BEFORE first register
SELECT * FROM pgokf.refresh_bundle(1);                   -- or re-sync to populate sources
```

**Fix.** Set `store_source` before the first `register_bundle`, or run
`refresh_bundle` / re-register afterward so the source bytes are projected.

### `export_sources`: bad bundle or destination directory

```text
ERROR:  22023: bundle 999 is not registered
ERROR:  22023: resolved path /tmp/.../outside is outside allowed_roots
ERROR:  22023: dest_dir is not a directory: /srv/exports/out.tar
```

**Cause.** `export_sources(bundle_id, dest_dir)` reuses `export_parquet`'s
destination validation: `dest_dir` must be an existing directory, canonical and
traversal-free, and - when `allowed_roots` is configured - contained
within an allowed root. Files are created with `O_NOFOLLOW`, so a **symlink**
planted at a target path is refused with `22023` rather than followed. **Fix.**
Pass an existing, writable directory the server's OS user can reach, under an
allowed root if `allowed_roots` is set; remove any symlink at a colliding
target name.

---

## `23505` - bundle already registered

```text
ERROR:  23505: bundle path /srv/okf-bundles/handbook is already registered; use pgokf.refresh_bundle
```

**Cause.** The **canonical** path is already registered. Registration is keyed on
the canonicalized path, so two paths that resolve to the same directory (e.g. via
a symlink) collide. The key is per tenant (`UNIQUE (tenant_id, path)`), so a
different tenant registering the same path does not collide. **Fix.**
Re-synchronize the existing bundle instead of re-registering it:

```sql
SELECT id, path FROM pgokf.list_bundles();     -- find the bundle_id
SELECT * FROM pgokf.refresh_bundle(1);
```

---

## Concurrency: a bundle appears "locked"

`register_bundle`, `refresh_bundle`, and `unregister_bundle` serialize on a
**bundle-scoped transaction advisory lock** keyed on the canonical path. If one
of these is running in another session, a second operation on the *same* bundle
blocks until the first transaction commits or rolls back (operations on
*different* bundles proceed in parallel). This is expected serialization, not an
error - the second call proceeds once the first finishes. If a call seems stuck,
look for a long-running transaction holding the lock:

```sql
SELECT pid, query, state, wait_event_type, wait_event
FROM pg_stat_activity
WHERE query ILIKE '%pgokf.%bundle%' AND pid <> pg_backend_pid();
```

---

## `XX000` - internal error

```text
ERROR:  XX000: …
```

**Cause.** A broken installation invariant - for example a composite result type
that cannot be resolved, or a `NOT NULL` catalog column observed as `NULL`. This
should not occur in normal operation. **Fix.** Confirm the installed SQL and the
loaded module agree (`SELECT pgokf.version();`), and reinstall the extension if
they diverge (`cargo pgrx install …` for the target major version). If it
persists, capture the full `VERBOSITY verbose` output and the bundle-relative
path from the message.

### `export_sources`: stored source fails its hash check

```text
ERROR:  XX000: stored source for runbooks/failover.md does not match its recorded file hash (expected <blake3>, computed <blake3>)
```

**Cause.** `export_sources` verifies every stored source against the concept's
recorded BLAKE3 `file_hash` **before** any file is created, so a mismatch aborts
the whole export and nothing is written. This is a corruption / integrity
condition (the stored bytes drifted from their recorded digest), not caller
input - hence `XX000`, not `22023`. **Fix.** Re-`refresh_bundle` the affected
bundle to re-project the source bytes from the on-disk originals, then re-run
the export. If it recurs, the on-disk bundle or the storage underneath the
catalog is corrupt.

---

## Nothing returned from `concept_search`

Not an error - a few benign causes:

- **The bundle is not active.** Search skips disabled bundles
  (`pgokf.bundles.enabled` false) and retired ones (`retired_at` set). Check
  with `SELECT id, enabled FROM pgokf.list_bundles();` and, for retired bundles
  (hidden from `list_bundles`), `SELECT * FROM pgokf.catalog_stats();`.
  `unretire_bundle` / `set_bundle_enabled` restore visibility.
- **The session declares a different tenant.** With the `pgokf.tenant` GUC set,
  search sees only that tenant's rows. Inspect it with `SHOW pgokf.tenant;`
  and clear it with `RESET pgokf.tenant;` to compare.
- **The query does not match the weighted vector.** Matching uses
  `websearch_to_tsquery(<configured text-search config>, …)` - the
  `default_text_search_config` setting (default `pg_catalog.english`), so
  stemming and stop-words follow that configuration. Note that changing the
  config does not re-index already-synced bundles (see `configuration.md`). Try
  broader terms, and remember exact metadata filters
  (`pgokf.concepts.tags`, `type`) remain available regardless of language.
- **Nothing was ingested.** Confirm `file_count > 0` via `pgokf.bundle_info`.
  Reserved files (`index.md`, `log.md`) do not become concepts.

---

## Exported Parquet timestamps look like large integers

Not an error. `export_parquet` writes the OKF v0.2 provenance timestamp
`generated_at` as **epoch microseconds** cast to `bigint`, which the Parquet
writer stores as a `Timestamp(µs, UTC)` logical type. A reader that ignores the
logical type sees a large integer (microseconds since 1970-01-01 UTC). In
DuckDB the column reads back as a native `TIMESTAMP`:

```sql
SELECT concept_id, generated_at
FROM read_parquet('/srv/exports/concept_provenance.parquet');
-- generated_at is a TIMESTAMP; to force from raw µs: make_timestamp(generated_at)
```

This epoch cast is what makes the export interoperable - an OKF v0.2 ISO 8601
instant round-trips through the catalog's `timestamptz` column into a portable
Parquet timestamp.
