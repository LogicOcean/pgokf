# pgokf SQL API reference

Complete reference for every object `CREATE EXTENSION pgokf;` installs: functions,
composite types, tables, roles, and GUCs. Everything the extension exposes lives
in the non-relocatable `pgokf` schema, except the administrator-only
`pgokf_private.config` table.

Every signature, volatility, security attribute, and required role below is taken
verbatim from the extension source and the generated install SQL, and was
exercised against a live PostgreSQL 18 cluster.

## Conventions

- **Volatility** is the PostgreSQL function volatility class (`IMMUTABLE`,
  `STABLE`, `VOLATILE`).
- **Security** is either *invoker rights* (the default; the function runs as the
  caller) or `SECURITY DEFINER` (the function runs as the extension owner with a
  pinned `search_path = pg_catalog, pg_temp`). See
  [security.md](security.md) for why each choice was made.
- **Required role** is the membership enforced by both the SQL `EXECUTE` grant and
  an in-function role check (`pgokf_reader` or `pgokf_admin`).
- SQLSTATEs raised: `22023` invalid parameter, `42501` insufficient privilege,
  `23505` unique violation (duplicate registration), `XX000` internal error. See
  [troubleshooting.md](troubleshooting.md).

## Function summary

| Function | Returns | Volatility | Security | Required role |
| -------- | ------- | ---------- | -------- | ------------- |
| `version()` | `text` | IMMUTABLE | invoker | `pgokf_reader` |
| `register_bundle(path, name, options)` | `bundle_sync_result` | VOLATILE | DEFINER | `pgokf_admin` |
| `refresh_bundle(bundle_id)` | `bundle_sync_result` | VOLATILE | DEFINER | `pgokf_admin` |
| `unregister_bundle(bundle_id)` | `bundle_info` | VOLATILE | DEFINER | `pgokf_admin` |
| `list_bundles()` | `SETOF bundle_info` | STABLE | invoker | `pgokf_reader` |
| `bundle_info(bundle_id)` | `bundle_info` | STABLE | invoker | `pgokf_reader` |
| `concept_search(query, bundle_id, limit_count)` | `SETOF concept_search_result` | STABLE | invoker | `pgokf_reader` |
| `concept_neighbors(concept_id, max_hops, bundle_id)` | `SETOF concept_neighbor` | STABLE | invoker | `pgokf_reader` |
| `set_config(key, value)` | `void` | VOLATILE | DEFINER | `pgokf_admin` |
| `reset_config(key)` | `void` | VOLATILE | DEFINER | `pgokf_admin` |
| `get_config()` | `jsonb` | VOLATILE | DEFINER | `pgokf_reader` |
| `export_parquet(bundle_id, dest_dir)` | `export_result` | VOLATILE | DEFINER | `pgokf_admin` |

`register_bundle`, `concept_search`, `concept_neighbors`, and `reset_config`
accept `NULL`-defaulting arguments and are therefore **not** declared `STRICT`;
every other function — including `list_bundles` and `bundle_info`, which take no
`NULL`-defaulting argument — is `STRICT`. `concept_search` and
`concept_neighbors` are also `PARALLEL SAFE`.

---

## Bundle lifecycle

### `pgokf.version() → text`

Report the version of the loaded `pgokf` shared library (the crate version).
`IMMUTABLE STRICT PARALLEL SAFE`, invoker rights. Although the function itself
carries no role check, `USAGE` on schema `pgokf` is revoked from `PUBLIC`, so a
caller needs membership in `pgokf_reader` or `pgokf_admin` (or superuser);
a role with neither gets `42501` (`permission denied for schema pgokf`). Useful
to confirm the installed SQL and the loaded module agree after an upgrade.

```sql
SELECT pgokf.version();
--  version
-- ---------
--  0.1.0
```

### `pgokf.register_bundle(path text, name text DEFAULT NULL, options jsonb DEFAULT '{}') → pgokf.bundle_sync_result`

Register an OKF bundle root and synchronize it into the catalog. `VOLATILE`,
`SECURITY DEFINER`, **requires `pgokf_admin`**.

| Parameter | Type | Default | Meaning |
| --------- | ---- | ------- | ------- |
| `path` | `text` | — | Absolute, traversal-free, canonicalizable server-side directory. |
| `name` | `text` | `NULL` | Optional human label stored on `pgokf.bundles.name`. |
| `options` | `jsonb` | `'{}'` | Stored verbatim on `pgokf.bundles.options` for producer use. |

Behavior:

- The path is validated (absolute, no `..`, no NUL), canonicalized, and confirmed
  to be a directory. When `allowed_roots` is configured the resolved path must
  fall inside one of them (see [configuration.md](configuration.md)).
- The canonical path must not already be registered — a duplicate raises
  `23505`; use `refresh_bundle` to re-synchronize instead.
- Discovery is symlink-escape safe and bounded by the `pgokf.*` GUCs
  (`max_file_bytes`, `max_bundle_files`, `max_frontmatter_bytes`). Reserved
  files (`index.md`, `log.md`) at any depth are skipped.
- Parsing is strict: the first malformed file aborts the whole sync (`22023`) and
  the surrounding transaction rolls back, so a partial projection is never
  committed.
- Returns a one-row `bundle_sync_result` with per-bucket counts.

```sql
SELECT * FROM pgokf.register_bundle('/abs/path/to/examples/sample-bundle');
--  bundle_id |                   path                   | added | updated | removed | unchanged | total
-- -----------+------------------------------------------+-------+---------+---------+-----------+-------
--          1 | /abs/path/to/examples/sample-bundle      |     4 |       0 |       0 |         0 |     4
```

### `pgokf.refresh_bundle(bundle_id bigint) → pgokf.bundle_sync_result`

Incrementally re-synchronize a registered bundle from its stored canonical path.
`VOLATILE STRICT`, `SECURITY DEFINER`, **requires `pgokf_admin`**.

Only files whose BLAKE3 content hash changed are re-parsed; unchanged rows are
left untouched (preserving their `indexed_at`), and rows for deleted files are
removed. An unknown `bundle_id` raises `22023`. A concurrent register/refresh of
the same bundle serializes on a bundle-scoped advisory lock.

```sql
SELECT added, updated, removed, unchanged, total FROM pgokf.refresh_bundle(1);
--  added | updated | removed | unchanged | total
-- -------+---------+---------+-----------+-------
--      0 |       0 |       0 |         4 |     4
```

### `pgokf.unregister_bundle(bundle_id bigint) → pgokf.bundle_info`

Delete a bundle and return the removed bundle's `bundle_info`. `VOLATILE STRICT`,
`SECURITY DEFINER`, **requires `pgokf_admin`**.

Serializes on the bundle advisory lock, then deletes the `pgokf.bundles` row;
concepts, metadata, links, and provenance cascade through their foreign keys.
An unknown `bundle_id` raises `22023`.

```sql
SELECT id, path, file_count FROM pgokf.unregister_bundle(1);
```

### `pgokf.list_bundles() → SETOF pgokf.bundle_info`

List every registered bundle, ordered by `id`. `STABLE`, invoker rights,
**requires `pgokf_reader`**.

```sql
SELECT id, path, name, file_count, enabled FROM pgokf.list_bundles();
--  id |                   path                   | name | file_count | enabled
-- ----+------------------------------------------+------+------------+---------
--   1 | /abs/path/to/examples/sample-bundle      |      |          4 | t
```

### `pgokf.bundle_info(bundle_id bigint) → pgokf.bundle_info`

Return one registered bundle as `bundle_info`. `STABLE STRICT`, invoker rights,
**requires `pgokf_reader`**. An unknown `bundle_id` raises `22023`.

```sql
SELECT id, file_count, last_synced_at FROM pgokf.bundle_info(1);
```

---

## Search

### `pgokf.concept_search(query text, bundle_id bigint DEFAULT NULL, limit_count int DEFAULT 20) → SETOF pgokf.concept_search_result`

Rank catalog concepts against a `websearch_to_tsquery` query over the weighted
`body_tsv` column. `STABLE PARALLEL SAFE`, invoker rights, **requires
`pgokf_reader`**.

| Parameter | Type | Default | Meaning |
| --------- | ---- | ------- | ------- |
| `query` | `text` | — | Free-text query; must contain a non-whitespace character (`22023` otherwise). |
| `bundle_id` | `bigint` | `NULL` | Scope the search to one bundle; `NULL` searches all enabled bundles. |
| `limit_count` | `int` | `20` | Maximum hits; must be in `1..=500` (`22023` otherwise). |

Details:

- Matching uses `websearch_to_tsquery('pg_catalog.english', query)`; ranking uses
  `ts_rank_cd`. Weights are title `A`, tags/type/description `B`, body `D`.
- Only **enabled** bundles are searched (`pgokf.bundles.enabled`).
- Each hit carries a `ts_headline` snippet over title, description, and body.
- Rows are ordered by descending rank, then ascending `concept_id` as a stable
  tiebreaker. Ranks are comparable **only within one query** — order by them,
  never persist them.

```sql
SELECT concept_id, title, type, round(rank::numeric, 4) AS rank
FROM pgokf.concept_search('postgres failover');
--          concept_id         |       title        |   type    |  rank
-- ----------------------------+--------------------+-----------+--------
--  runbooks/database-failover | Database failover  | Runbook   | 0.5808
--  runbooks/appendix          | Failover appendix  | Reference | 0.3357
--  services/postgresql        | PostgreSQL service | Reference | 0.0067
```

Filter or join the result with ordinary SQL — for example to recover columns
`concept_search` does not return (`tags`, `description`):

```sql
SELECT s.concept_id, s.rank, c.tags
FROM pgokf.concept_search('incident response') AS s
JOIN pgokf.concepts AS c
  ON c.bundle_id = s.bundle_id AND c.id = s.concept_id
WHERE c.type = 'Runbook'
ORDER BY s.rank DESC, s.concept_id ASC;
```

---

## Graph

### `pgokf.concept_neighbors(concept_id text, max_hops int DEFAULT 2, bundle_id bigint DEFAULT NULL) → SETOF pgokf.concept_neighbor`

Walk the resolved internal link graph outward from a concept. `STABLE PARALLEL
SAFE`, invoker rights, **requires `pgokf_reader`**.

| Parameter | Type | Default | Meaning |
| --------- | ---- | ------- | ------- |
| `concept_id` | `text` | — | Start concept (path-derived ID, no `.md`). |
| `max_hops` | `int` | `2` | Maximum traversal depth; must be `>= 1` (`22023` otherwise); capped at `pgokf.max_graph_hops`. |
| `bundle_id` | `bigint` | `NULL` | Scope to one bundle. When `NULL` and the ID exists in more than one bundle, the call raises `22023` asking you to disambiguate. |

The traversal is a cycle-safe recursive CTE over `pgokf.links`. **Only resolved,
non-external edges** are followed (`resolved AND NOT is_external`); external and
unresolved links never become edges. Each reachable concept is returned once,
with the shortest hop count and the path taken. A start concept that exists in
no bundle yields an empty set.

```sql
SELECT source_id, neighbor_id, hops, path, title
FROM pgokf.concept_neighbors('runbooks/database-failover', 3, 1)
ORDER BY hops, neighbor_id;
--          source_id          |     neighbor_id     | hops |                       path                       |       title
-- ----------------------------+---------------------+------+--------------------------------------------------+--------------------
--  runbooks/database-failover | dashboards/health   |    1 | {runbooks/database-failover,dashboards/health}   | Service health
--  runbooks/database-failover | runbooks/appendix   |    1 | {runbooks/database-failover,runbooks/appendix}   | Failover appendix
--  runbooks/database-failover | services/postgresql |    1 | {runbooks/database-failover,services/postgresql} | PostgreSQL service
```

The `pgokf.links` table (below) supports direct edge and backlink queries; see
[`examples/queries/graph.sql`](../examples/queries/graph.sql).

---

## Configuration functions

These manage the durable policy row in `pgokf_private.config`. See
[configuration.md](configuration.md) for the full key catalog, defaults, and
which keys the current engine actually consults.

### `pgokf.set_config(key text, value jsonb) → void`

Set one durable configuration key from a validated, coerced `jsonb` value.
`VOLATILE STRICT`, `SECURITY DEFINER`, **requires `pgokf_admin`**.

`value` shape per key: an array of strings for `allowed_roots` /
`default_exclude`, a boolean for `default_strict`, an integer for
`sync_log_retention_days`, a string for `default_text_search_config`. Unknown
keys and wrong-shaped or out-of-domain values raise `22023`. A
`default_text_search_config` must name an installed configuration in
`pg_catalog.pg_ts_config`.

```sql
SELECT pgokf.set_config('allowed_roots', '["/srv/okf-bundles"]'::jsonb);
```

### `pgokf.reset_config(key text DEFAULT NULL) → void`

Reset one configuration key to its column default, or **every** key when `key`
is `NULL`. `VOLATILE`, `SECURITY DEFINER`, **requires `pgokf_admin`**.

```sql
SELECT pgokf.reset_config('allowed_roots');  -- reset one key
SELECT pgokf.reset_config();                 -- reset all keys
```

### `pgokf.get_config() → jsonb`

Return the effective catalog configuration as a `jsonb` object. `VOLATILE
STRICT`, `SECURITY DEFINER`, **requires `pgokf_reader`**.

```sql
SELECT jsonb_pretty(pgokf.get_config());
-- {
--     "allowed_roots": [],
--     "default_strict": true,
--     "default_exclude": [],
--     "sync_log_retention_days": 30,
--     "default_text_search_config": "pg_catalog.english"
-- }
```

---

## Export

### `pgokf.export_parquet(bundle_id bigint, dest_dir text) → pgokf.export_result`

Write a point-in-time Apache Parquet snapshot of one bundle's catalog projection
into a server-side directory, and return the per-file row counts and total bytes
written. `VOLATILE STRICT`, `SECURITY DEFINER`, **requires `pgokf_admin`**.

This is the **only** function in the extension that **writes files** to the
server filesystem; every other function reads. It is admin-only for exactly that
reason. See [security.md](security.md#server-side-file-writes-export_parquet) for
the write-side threat model.

| Parameter | Type | Meaning |
| --------- | ---- | ------- |
| `bundle_id` | `bigint` | The bundle to export; an unknown id raises `22023`. |
| `dest_dir` | `text` | Target directory. Must be absolute, NUL-free, and traversal-free; is canonicalized (symlinks resolved); must already exist and be writable; and, when `pgokf.allowed_roots` is configured, its canonical form must fall inside a configured root. The function never creates the directory and never writes outside it. |

Behavior:

- Writes exactly four files into `dest_dir`, one per catalog table for the
  requested bundle: `concepts.parquet`, `concept_metadata.parquet`,
  `links.parquet`, and `concept_provenance.parquet` (Zstandard-compressed). The
  `body_tsv` search vector is excluded (no portable Parquet representation);
  `timestamptz` columns are written as UTC microsecond timestamps and `jsonb` as
  its canonical JSON text.
- Streams each table in bounded keyset batches, so peak memory is independent of
  catalog size; every query is scoped to `bundle_id`, so no other bundle's rows
  can leak into the export.
- Raises `22023` for a missing bundle or a bad/missing/non-contained directory,
  and `42501` for a directory the server process cannot write.

```sql
-- Counts below are the sample bundle in examples/sample-bundle.
SELECT * FROM pgokf.export_parquet(1, '/srv/okf-exports/sample');
--  bundle_id |         dest_dir         | concepts_rows | metadata_rows | links_rows | provenance_rows | bytes_written
-- -----------+--------------------------+---------------+---------------+------------+-----------------+---------------
--          1 | /srv/okf-exports/sample  |             4 |             9 |         12 |               4 |         14330
```

---

## Composite types

### `pgokf.bundle_sync_result`

Returned by `register_bundle` and `refresh_bundle`.

| Column | Type | Meaning |
| ------ | ---- | ------- |
| `bundle_id` | `bigint` | Registered bundle identity. |
| `path` | `text` | Canonical bundle path. |
| `added` | `integer` | Newly inserted concepts. |
| `updated` | `integer` | Content-changed concepts re-parsed. |
| `removed` | `integer` | Concepts removed for deleted files. |
| `unchanged` | `integer` | Concepts left untouched (hash matched). |
| `total` | `integer` | Concept count after the sync. |

### `pgokf.concept_search_result`

Returned by `concept_search`. Note the search key is `concept_id`, **not** `id`,
and there is **no** `tags` column — join `pgokf.concepts` to recover it.

| Column | Type | Meaning |
| ------ | ---- | ------- |
| `bundle_id` | `bigint` | Bundle the hit belongs to. |
| `concept_id` | `text` | Path-derived concept ID. |
| `path` | `text` | Bundle-relative source path. |
| `title` | `text` | Concept title (may be `NULL`). |
| `type` | `text` | OKF concept type (may be `NULL`). |
| `rank` | `real` | `ts_rank_cd` score; comparable only within one query. |
| `headline` | `text` | `ts_headline` snippet (may be `NULL`). |

### `pgokf.bundle_info`

Returned by `list_bundles`, `bundle_info`, and `unregister_bundle`.

| Column | Type | Meaning |
| ------ | ---- | ------- |
| `id` | `bigint` | Bundle identity. |
| `path` | `text` | Canonical bundle path. |
| `name` | `text` | Optional label (may be `NULL`). |
| `okf_version` | `text` | Bundle OKF version; currently always `NULL` (not yet populated by the engine). |
| `file_count` | `integer` | Concept count. |
| `last_synced_at` | `timestamptz` | Last successful sync (may be `NULL`). |
| `enabled` | `boolean` | Whether the bundle is searched. |

### `pgokf.export_result`

Returned by `export_parquet`. One row summarizing the snapshot just written.

| Column | Type | Meaning |
| ------ | ---- | ------- |
| `bundle_id` | `bigint` | The exported bundle's identity. |
| `dest_dir` | `text` | The resolved (canonical) destination directory. |
| `concepts_rows` | `bigint` | Rows written to `concepts.parquet`. |
| `metadata_rows` | `bigint` | Rows written to `concept_metadata.parquet`. |
| `links_rows` | `bigint` | Rows written to `links.parquet`. |
| `provenance_rows` | `bigint` | Rows written to `concept_provenance.parquet`. |
| `bytes_written` | `bigint` | Total bytes across the four Parquet files. |

### `pgokf.concept_neighbor`

Returned by `concept_neighbors`.

| Column | Type | Meaning |
| ------ | ---- | ------- |
| `source_id` | `text` | The start concept every path originates from. |
| `neighbor_id` | `text` | A reachable concept. |
| `hops` | `integer` | Shortest number of resolved edges to the neighbor. |
| `path` | `text[]` | Concept IDs on the shortest path, start through neighbor. |
| `title` | `text` | Neighbor title (may be `NULL`). |

---

## Tables

Readers hold `SELECT` on `pgokf.bundles`, `pgokf.concepts`,
`pgokf.concept_metadata`, `pgokf.links`, and `pgokf.concept_provenance`. All
writes go through the `SECURITY DEFINER` sync/admin functions — no role has
direct DML. `pgokf_private.config` is reachable only through the config
functions.

### `pgokf.bundles`

One registered OKF bundle root.

| Column | Type | Notes |
| ------ | ---- | ----- |
| `id` | `bigint` | Primary key, `GENERATED ALWAYS AS IDENTITY`. |
| `path` | `text` | `NOT NULL`, `UNIQUE` — the canonical path. |
| `name` | `text` | Optional label. |
| `okf_version` | `text` | Currently always `NULL`. |
| `file_count` | `integer` | `NOT NULL DEFAULT 0`. |
| `last_synced_at` | `timestamptz` | Last successful sync. |
| `sync_hash` | `text` | Aggregate BLAKE3 digest over sorted `(path, file_hash)` pairs of the last sync. |
| `options` | `jsonb` | `NOT NULL DEFAULT '{}'` — producer options from `register_bundle`. |
| `enabled` | `boolean` | `NOT NULL DEFAULT true` — search skips disabled bundles. |

### `pgokf.concepts`

One row per `(bundle_id, id)` — the projection of one OKF concept document.

| Column | Type | Notes |
| ------ | ---- | ----- |
| `bundle_id` | `bigint` | `NOT NULL`, FK to `pgokf.bundles(id)` `ON DELETE CASCADE`. |
| `id` | `text` | Path-derived concept ID (bundle-relative path without `.md`). |
| `path` | `text` | `NOT NULL` — bundle-relative source path. |
| `type` | `text` | OKF concept type. |
| `title` | `text` | Concept title. |
| `description` | `text` | Optional short description. |
| `tags` | `text[]` | Frontmatter tags in declaration order. |
| `resource` | `text` | Frontmatter `resource`, serialized as JSON text. |
| `body_text` | `text` | `NOT NULL DEFAULT ''` — Markdown body as compact plain text. |
| `file_hash` | `text` | `NOT NULL` — BLAKE3 digest; the incremental-sync identity. |
| `modified_at` | `timestamptz` | Filesystem mtime, when reported. |
| `body_tsv` | `tsvector` | Weighted search vector (title A, tags/type/description B, body D). |
| `indexed_at` | `timestamptz` | `NOT NULL DEFAULT now()` — refreshed only when the concept changes. |

Keys: primary key `(bundle_id, id)`; unique `(bundle_id, path)`. Indexes:
GIN on `tags`, GIN on `body_tsv`, btree on `type`, btree on `path`.

### `pgokf.concept_metadata`

Producer-defined frontmatter keys, one row per key, retained as `jsonb`. Keys
not recognized by the typed columns land here losslessly.

| Column | Type | Notes |
| ------ | ---- | ----- |
| `bundle_id` | `bigint` | `NOT NULL`, part of FK to `pgokf.concepts`. |
| `concept_id` | `text` | `NOT NULL`, part of FK to `pgokf.concepts`. |
| `key` | `text` | `NOT NULL` — frontmatter key. |
| `value` | `jsonb` | `NOT NULL` — the key's value. |

Unique `(bundle_id, concept_id, key)`; FK `(bundle_id, concept_id)` to
`pgokf.concepts(bundle_id, id)` `ON DELETE CASCADE`. GIN index on
`value jsonb_path_ops`.

### `pgokf.links`

Directed Markdown links extracted per concept, one row per outgoing link.

| Column | Type | Notes |
| ------ | ---- | ----- |
| `bundle_id` | `bigint` | `NOT NULL`, part of FK to `pgokf.concepts`. |
| `source_id` | `text` | `NOT NULL` — concept the link came from. |
| `target_id` | `text` | Internal destination concept ID; `NULL` for external links. |
| `link_text` | `text` | Plain-text label of the link. |
| `target_path` | `text` | Normalized bundle-relative destination path (with `.md`) for internal links; `NULL` for external. |
| `link_kind` | `text` | `NOT NULL` — `inline`, `reference`, `autolink`, `email`, or `image`. |
| `resolved` | `boolean` | `NOT NULL DEFAULT false` — true only for an internal link whose target concept exists in the same bundle. |
| `is_external` | `boolean` | `NOT NULL DEFAULT false` — true for scheme-qualified / protocol-relative / email destinations. |
| `ordinal` | `integer` | `NOT NULL` — zero-based document-order position. |

Primary key `(bundle_id, source_id, ordinal)`; FK `(bundle_id, source_id)` to
`pgokf.concepts(bundle_id, id)` `ON DELETE CASCADE`. Index on
`(bundle_id, target_id)`. Unresolved internal links and external links are
retained (OKF permits broken links).

### `pgokf.concept_provenance`

Sparse projection of OKF v0.2 provenance/trust/lifecycle frontmatter. Only
concepts that carry such frontmatter get a row.

| Column | Type | Notes |
| ------ | ---- | ----- |
| `bundle_id` | `bigint` | `NOT NULL`, part of FK to `pgokf.concepts`. |
| `concept_id` | `text` | `NOT NULL`, part of FK to `pgokf.concepts`. |
| `generated_by` | `text` | From `generated_by`, bare `generated`, or `generated.by`. |
| `verified` | `boolean` | Truthy `verified` flag, or a non-empty set of verification records. |
| `verification_method` | `text` | From `verification_method`, bare `verification`, or `verification.method`. |
| `freshness` | `text` | From `freshness`, falling back to the lifecycle `status`. |
| `details` | `jsonb` | `NOT NULL DEFAULT '{}'` — lossless copy of every recognized provenance key (`sources`, `generated`, `verified`, `usage_window`, `stale_after`, `parameters`, …). |

Primary key `(bundle_id, concept_id)`; FK to `pgokf.concepts` `ON DELETE
CASCADE`. Partial index on `verified WHERE verified`.

```sql
SELECT concept_id, generated_by, verified, verification_method, freshness
FROM pgokf.concept_provenance
ORDER BY concept_id;
```

### `pgokf_private.config`

Cluster-persistent policy: a single row, managed only through `set_config` /
`reset_config`. No role has direct DML. See
[configuration.md](configuration.md) for column semantics and defaults.

| Column | Type | Default |
| ------ | ---- | ------- |
| `singleton` | `boolean` | `true` (primary key; `CHECK (singleton)` pins one row) |
| `allowed_roots` | `text[]` | `'{}'` |
| `default_text_search_config` | `text` | `'pg_catalog.english'` |
| `default_strict` | `boolean` | `true` |
| `sync_log_retention_days` | `integer` | `30` (`CHECK >= 0`) |
| `default_exclude` | `text[]` | `'{}'` |

---

## Roles

| Role | Login | Grants |
| ---- | ----- | ------ |
| `pgokf_reader` | `NOLOGIN` | `USAGE` on schema `pgokf`; `SELECT` on the projection tables; `EXECUTE` on search/graph/list/`get_config`. |
| `pgokf_admin` | `NOLOGIN` | Everything `pgokf_reader` has (it is `GRANT`ed `pgokf_reader`), plus `USAGE` on `pgokf_private` and `EXECUTE` on `register_bundle`, `refresh_bundle`, `unregister_bundle`, `set_config`, `reset_config`, `export_parquet`. |

Both are cluster-wide roles created idempotently at extension install. Grant them
to real login users:

```sql
GRANT pgokf_reader TO analytics_ro;
GRANT pgokf_admin  TO catalog_ops;
```

See [security.md](security.md) for the authorization model.

---

## GUCs

Five `pgokf.*` server settings. The four resource ceilings use the `SIGHUP`
context — settable only in `postgresql.conf` plus a reload, **never** from a SQL
`SET`, so they stay trustworthy as hard safety limits. `log_level` uses `SUSET`,
so a superuser can change it at runtime. Full detail in
[configuration.md](configuration.md).

| GUC | Type | Default | Range | Context |
| --- | ---- | ------- | ----- | ------- |
| `pgokf.max_file_bytes` | integer | `4194304` (4 MiB) | `1 .. 2147483647` | `SIGHUP` |
| `pgokf.max_bundle_files` | integer | `100000` | `1 .. 2147483647` | `SIGHUP` |
| `pgokf.max_frontmatter_bytes` | integer | `262144` (256 KiB) | `1 .. 2147483647` | `SIGHUP` |
| `pgokf.max_graph_hops` | integer | `5` | `1 .. 1000` | `SIGHUP` |
| `pgokf.log_level` | string | `warning` | — | `SUSET` |

```sql
SHOW pgokf.max_graph_hops;
```
