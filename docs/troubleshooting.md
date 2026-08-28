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
| `22023` | invalid parameter value | bad path, malformed bundle content, out-of-range argument, invalid/unknown configuration, unknown/ambiguous identifier |
| `42501` | insufficient privilege | missing role membership or `EXECUTE` grant |
| `23505` | unique violation | registering an already-registered bundle path |
| `XX000` | internal error | a broken installation invariant (should not occur in normal use) |

All error strings below were produced against a live cluster.

---

## `42501` — permission denied

### `permission denied for function register_bundle` (or another function)

```text
ERROR:  42501: permission denied for function register_bundle
ERROR:  42501: permission denied for function concept_search
```

**Cause.** The current login user is not a member of the role that owns the
operation: `pgokf_admin` for the mutators and file-writing exports
(`register_bundle`, `refresh_bundle`, `unregister_bundle`, `set_config`,
`reset_config`, `export_parquet`, `export_sources`), or `pgokf_reader` for the
read paths (`concept_search`, `concept_neighbors`, `list_bundles`,
`bundle_info`, `get_config`, `get_concept_source`).

**Fix.** Grant the appropriate role to the user:

```sql
GRANT pgokf_reader TO analytics_ro;   -- read + search
GRANT pgokf_admin  TO catalog_ops;    -- register/refresh/configure (inherits reader)
```

Confirm membership with `\du` or:

```sql
SELECT pg_has_role('analytics_ro', 'pgokf_reader', 'MEMBER');
```

### `permission denied for schema pgokf`

**Cause.** The user has no `USAGE` on the schema (they were not granted either
role). **Fix.** Grant `pgokf_reader` or `pgokf_admin` as above — both carry
schema `USAGE`.

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

## `22023` — invalid parameter

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

**Cause.** Sync is strict — the **first** malformed concept file aborts the whole
sync and the transaction rolls back, so a partial projection is never committed.
Common causes: a file that does not begin with a `---` frontmatter delimiter,
unterminated frontmatter, invalid YAML, or a missing **required** field
(`type` and `title` are required). The offending file appears in the
`[bundle-relative path: …]` suffix.

**Fix.** Correct the named file (add a valid `---` YAML block with `type` and
`title`), then re-run `register_bundle` / `refresh_bundle`. Note that
`index.md` and `log.md` are reserved and skipped — they are never parsed as
concepts, so they cannot cause this error.

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

**Cause.** The concept ID exists in more than one bundle and no `bundle_id` was
given. **Fix.** Pass the third argument:
`SELECT * FROM pgokf.concept_neighbors('runbooks/failover', 2, 1);`.

### Unknown bundle

```text
ERROR:  22023: bundle 999 is not registered
```

**Cause.** `refresh_bundle`, `unregister_bundle`, or `bundle_info` was given a
`bundle_id` that does not exist. **Fix.** Look up the real ID with
`SELECT id, path FROM pgokf.list_bundles();`.

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
  only metadata and search were projected — there is nothing to return. This is
  the deployment tier where originals live in a data lake / mounted bucket.
- **No such concept.** The `(bundle_id, concept_id)` pair does not exist at all
  (wrong id, wrong bundle, or the concept was removed on a later refresh).

**Fix.** For the first case, enable source storage and re-register (the setting
is **not retroactive** — see below), or read the original from wherever the data
lake keeps it. For the second, look up the real id and bundle:

```sql
SELECT bundle_id, id FROM pgokf.concepts WHERE id = 'runbooks/failover';
```

### `store_source` is not retroactive

Enabling `store_source` after a bundle is already synced does **not** backfill
the stored bytes — like `default_text_search_config`, it is read at sync time.
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
traversal-free, and — when `pgokf.allowed_roots` is configured — contained
within an allowed root. Files are created with `O_NOFOLLOW`, so a **symlink**
planted at a target path is refused with `22023` rather than followed. **Fix.**
Pass an existing, writable directory the server's OS user can reach, under an
allowed root if `allowed_roots` is set; remove any symlink at a colliding
target name.

---

## `23505` — bundle already registered

```text
ERROR:  23505: bundle path /srv/okf-bundles/handbook is already registered; use pgokf.refresh_bundle
```

**Cause.** The **canonical** path is already registered. Registration is keyed on
the canonicalized path, so two paths that resolve to the same directory (e.g. via
a symlink) collide. **Fix.** Re-synchronize the existing bundle instead of
re-registering it:

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
error — the second call proceeds once the first finishes. If a call seems stuck,
look for a long-running transaction holding the lock:

```sql
SELECT pid, query, state, wait_event_type, wait_event
FROM pg_stat_activity
WHERE query ILIKE '%pgokf.%bundle%' AND pid <> pg_backend_pid();
```

---

## `XX000` — internal error

```text
ERROR:  XX000: …
```

**Cause.** A broken installation invariant — for example a composite result type
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
input — hence `XX000`, not `22023`. **Fix.** Re-`refresh_bundle` the affected
bundle to re-project the source bytes from the on-disk originals, then re-run
the export. If it recurs, the on-disk bundle or the storage underneath the
catalog is corrupt.

---

## Nothing returned from `concept_search`

Not an error — a few benign causes:

- **The bundle is disabled.** Search skips bundles where
  `pgokf.bundles.enabled` is false. Check with `SELECT id, enabled FROM
  pgokf.list_bundles();`.
- **The query does not match the weighted vector.** Matching uses
  `websearch_to_tsquery(<configured text-search config>, …)` — the
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

This epoch cast is what makes the export interoperable — an OKF v0.2 ISO 8601
instant round-trips through the catalog's `timestamptz` column into a portable
Parquet timestamp.
