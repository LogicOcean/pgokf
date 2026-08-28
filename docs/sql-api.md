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
- **Required role** is the *minimum* membership enforced by both the SQL `EXECUTE`
  grant and an in-function role check, on the tier hierarchy
  `pgokf_reader` < `pgokf_writer` < `pgokf_admin`. Because each tier inherits the
  one below, a higher tier always satisfies a lower requirement (an admin can do
  anything a writer or reader can).
- SQLSTATEs raised: `22023` invalid parameter, `42501` insufficient privilege,
  `23505` unique violation (duplicate registration), `XX000` internal error. See
  [troubleshooting.md](troubleshooting.md).

## Function summary

| Function | Returns | Volatility | Security | Required role |
| -------- | ------- | ---------- | -------- | ------------- |
| `version()` | `text` | IMMUTABLE | invoker | `pgokf_reader` |
| `register_bundle(path, name, options)` | `bundle_sync_result` | VOLATILE | DEFINER | `pgokf_writer` |
| `register_bundle_content(name, paths, contents, options)` | `bundle_sync_result` | VOLATILE | DEFINER | `pgokf_writer` |
| `refresh_bundle(bundle_id)` | `bundle_sync_result` | VOLATILE | DEFINER | `pgokf_writer` |
| `unregister_bundle(bundle_id)` | `bundle_info` | VOLATILE | DEFINER | `pgokf_writer` |
| `set_bundle_enabled(bundle_id, enabled)` | `bundle_info` | VOLATILE | DEFINER | `pgokf_writer` |
| `list_bundles()` | `SETOF bundle_info` | STABLE | invoker | `pgokf_reader` |
| `bundle_info(bundle_id)` | `bundle_info` | STABLE | invoker | `pgokf_reader` |
| `concept_search(query, bundle_id, limit_count, concept_type, tags, status, trust_tier)` | `SETOF concept_search_result` | STABLE | invoker | `pgokf_reader` |
| `find_similar(concept_id, bundle_id, limit_count)` | `SETOF concept_search_result` | STABLE | invoker | `pgokf_reader` |
| `concept_search_semantic(query_embedding, bundle_id, limit_count)` | `SETOF concept_search_result` | STABLE | invoker | `pgokf_reader` |
| `concept_search_hybrid(query, query_embedding, bundle_id, limit_count)` | `SETOF concept_search_result` | STABLE | invoker | `pgokf_reader` |
| `set_concept_embedding(bundle_id, concept_id, embedding)` | `void` | VOLATILE | DEFINER | `pgokf_writer` |
| `rebuild_embedding_index()` | `boolean` | VOLATILE | DEFINER | `pgokf_admin` |
| `concept_neighbors(concept_id, max_hops, bundle_id)` | `SETOF concept_neighbor` | STABLE | invoker | `pgokf_reader` |
| `set_config(key, value)` | `void` | VOLATILE | DEFINER | `pgokf_admin` |
| `reset_config(key)` | `void` | VOLATILE | DEFINER | `pgokf_admin` |
| `get_config()` | `jsonb` | VOLATILE | DEFINER | `pgokf_reader` |
| `list_sync_log(bundle_id, max_rows)` | `SETOF sync_log_entry` | VOLATILE | DEFINER | `pgokf_reader` |
| `catalog_stats()` | `SETOF catalog_stat` | STABLE | invoker | `pgokf_reader` |
| `health()` | `jsonb` | STABLE | DEFINER | `pgokf_reader` |
| `stale_concepts(bundle_id, as_of)` | `SETOF stale_concept` | STABLE | invoker | `pgokf_reader` |
| `rebuild_search_index()` | `boolean` | VOLATILE | DEFINER | `pgokf_admin` |
| `export_parquet(bundle_id, dest_dir)` | `export_result` | VOLATILE | DEFINER | `pgokf_admin` |
| `get_concept_source(bundle_id, concept_id)` | `bytea` | STABLE | invoker | `pgokf_reader` |
| `export_sources(bundle_id, dest_dir)` | `export_result` | VOLATILE | DEFINER | `pgokf_admin` |

`register_bundle`, `concept_search`, `find_similar`,
`concept_search_semantic`, `concept_search_hybrid`, `concept_neighbors`,
`reset_config`, `list_sync_log`, and `stale_concepts` accept `NULL`-defaulting
arguments and are therefore **not** declared `STRICT`; every other function —
including `list_bundles`, `bundle_info`, `catalog_stats`, `health`,
`set_concept_embedding`, and `rebuild_embedding_index`, which take no
`NULL`-defaulting argument — is `STRICT`. `concept_search`, `find_similar`,
`concept_search_semantic`, `concept_search_hybrid`, `concept_neighbors`,
`catalog_stats`, and `stale_concepts` are also `PARALLEL SAFE`.

The `register_bundle` / `refresh_bundle` / `unregister_bundle` /
`set_bundle_enabled` ingestion functions require `pgokf_writer` (an admin
qualifies by inheritance).

---

## Bundle lifecycle

### `pgokf.version() → text`

Report the version of the loaded `pgokf` shared library (the crate version).
`IMMUTABLE STRICT PARALLEL SAFE`, invoker rights. Although the function itself
carries no role check, `USAGE` on schema `pgokf` is revoked from `PUBLIC`, so a
caller needs membership in `pgokf_reader` (which `pgokf_writer` and `pgokf_admin`
inherit) or superuser; a role with none of them gets `42501`
(`permission denied for schema pgokf`). Useful
to confirm the installed SQL and the loaded module agree after an upgrade.

```sql
SELECT pgokf.version();
--  version
-- ---------
--  0.1.0
```

### `pgokf.register_bundle(path text, name text DEFAULT NULL, options jsonb DEFAULT '{}') → pgokf.bundle_sync_result`

Register an OKF bundle root and synchronize it into the catalog. `VOLATILE`,
`SECURITY DEFINER`, **requires `pgokf_writer`**.

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

### `pgokf.register_bundle_content(name text, paths text[], contents bytea[], options jsonb DEFAULT '{}') → pgokf.bundle_sync_result`

Register or resynchronize a bundle from **in-memory content** rather than a
filesystem path — the *mountless* ingestion path. `VOLATILE`, `SECURITY
DEFINER`, **requires `pgokf_writer`**. The extension performs no network or
filesystem I/O here: a companion process (see the [`pgokf-ingest`
crate](deployment-topologies.md#enterprise-tier-mountless-the-ingestion-companion)) reads an
object store and streams the collected `(path, bytes)` pairs into this function.

| Parameter | Type | Default | Meaning |
| --------- | ---- | ------- | ------- |
| `name` | `text` | — | Logical bundle name. The bundle is keyed on the synthetic path `content:<name>` (`source_type = 'content'`), which cannot collide with an absolute filesystem path. |
| `paths` | `text[]` | — | Bundle-relative paths, one per element. Each must be relative, traversal-free (no `..`), and NUL-free. |
| `contents` | `bytea[]` | — | The bytes for each path; must be the **same length** as `paths`, with no `NULL` element. |
| `options` | `jsonb` | `'{}'` | Stored verbatim on `pgokf.bundles.options`. |

Behavior:

- Calling it again with the same `name` **resyncs**: contents are hashed
  (BLAKE3) and diffed against the stored projection exactly like a filesystem
  refresh — changed concepts are upserted, missing ones deleted, unchanged rows
  left untouched. This is how the companion re-ingests incrementally.
- The same discovery bounds apply: `max_bundle_files` / `max_file_bytes` are
  enforced on the provided content, and reserved files (`index.md`, `log.md`)
  are handled as in `register_bundle` (a root `index.md` supplies `okf_version`).
- A length mismatch, a `NULL` content element, or an unsafe path raises `22023`;
  the whole call is atomic under the bundle advisory lock.
- With `store_source` enabled, the provided bytes round-trip through
  `get_concept_source` / `export_sources` just like filesystem-sourced bundles.

```sql
SELECT added, total FROM pgokf.register_bundle_content(
    'handbook',
    ARRAY['runbooks/deploy.md'],
    ARRAY[convert_to(E'---\ntype: runbook\ntitle: Deploy\n---\nsteps', 'UTF8')::bytea]
);
--  added | total
-- -------+-------
--      1 |     1
```

### `pgokf.refresh_bundle(bundle_id bigint) → pgokf.bundle_sync_result`

Incrementally re-synchronize a **filesystem-sourced** bundle from its stored
canonical path. `VOLATILE STRICT`, `SECURITY DEFINER`, **requires
`pgokf_writer`**.

Only files whose BLAKE3 content hash changed are re-parsed; unchanged rows are
left untouched (preserving their `indexed_at`), and rows for deleted files are
removed. An unknown `bundle_id` raises `22023`. A concurrent register/refresh of
the same bundle serializes on a bundle-scoped advisory lock. A **content-sourced**
bundle (`source_type = 'content'`) has no filesystem root, so `refresh_bundle`
raises `22023` for it — re-sync those by calling `register_bundle_content` again.

```sql
SELECT added, updated, removed, unchanged, total FROM pgokf.refresh_bundle(1);
--  added | updated | removed | unchanged | total
-- -------+---------+---------+-----------+-------
--      0 |       0 |       0 |         4 |     4
```

### `pgokf.unregister_bundle(bundle_id bigint) → pgokf.bundle_info`

Delete a bundle and return the removed bundle's `bundle_info`. `VOLATILE STRICT`,
`SECURITY DEFINER`, **requires `pgokf_writer`**. Works for both filesystem- and
content-sourced bundles.

Serializes on the bundle advisory lock, then deletes the `pgokf.bundles` row;
concepts, metadata, links, and provenance cascade through their foreign keys.
An unknown `bundle_id` raises `22023`.

```sql
SELECT id, path, file_count FROM pgokf.unregister_bundle(1);
```

An unregister is recorded in the audit log (`op = 'unregister'`); see
[`pgokf.list_sync_log`](#pgokflist_sync_logbundle_id-bigint-default-null-max_rows-int-default-100--setof-pgokfsync_log_entry).

### `pgokf.set_bundle_enabled(bundle_id bigint, enabled boolean) → pgokf.bundle_info`

Enable or disable a registered bundle, returning the updated `bundle_info`.
`VOLATILE STRICT`, `SECURITY DEFINER`, **requires `pgokf_writer`**.

A disabled bundle's concepts are excluded from ranked search
(`pgokf.concept_search`) and graph traversal (`pgokf.concept_neighbors`) without
deleting any catalog rows, so the toggle is fully reversible — re-enabling
restores the bundle exactly. Serializes on the bundle advisory lock. An unknown
`bundle_id` raises `22023`.

```sql
SELECT id, enabled FROM pgokf.set_bundle_enabled(1, false);  -- hide bundle 1
SELECT id, enabled FROM pgokf.set_bundle_enabled(1, true);   -- and restore it
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

### `pgokf.concept_search(query text, bundle_id bigint DEFAULT NULL, limit_count int DEFAULT 20, concept_type text DEFAULT NULL, tags text[] DEFAULT NULL, status text DEFAULT NULL, trust_tier text DEFAULT NULL) → SETOF pgokf.concept_search_result`

Rank catalog concepts against a `websearch_to_tsquery` query over the weighted
`body_tsv` column, with optional structured filters. `STABLE PARALLEL SAFE`,
invoker rights, **requires `pgokf_reader`**.

| Parameter | Type | Default | Meaning |
| --------- | ---- | ------- | ------- |
| `query` | `text` | — | Free-text query; must contain a non-whitespace character (`22023` otherwise). |
| `bundle_id` | `bigint` | `NULL` | Scope the search to one bundle; `NULL` searches all enabled bundles. |
| `limit_count` | `int` | `20` | Maximum hits; must be in `1..=500` (`22023` otherwise). |
| `concept_type` | `text` | `NULL` | Keep only hits whose `type` equals this exactly. `NULL` = no filter. |
| `tags` | `text[]` | `NULL` | Keep only hits whose `tags` contain **every** listed tag (ALL-of, `tags @> filter`). `NULL` or empty = no filter. |
| `status` | `text` | `NULL` | Keep only hits whose `concept_provenance.status` equals this. `NULL` = no filter. |
| `trust_tier` | `text` | `NULL` | Keep only hits whose derived `concept_provenance.trust_tier` equals this (`unverified` / `machine-confirmed` / `human-reviewed`). `NULL` = no filter. |

> **Backward compatible.** The four trailing filters are optional and each a
> no-op when `NULL`, so the historical `concept_search(query, bundle_id,
> limit_count)` call is unchanged. Concepts with no `concept_provenance` row have
> a `NULL` `status`/`trust_tier` and are therefore excluded by a non-`NULL`
> `status`/`trust_tier` filter.

Details:

- Matching uses `websearch_to_tsquery(<cfg>, query)`; ranking uses `ts_rank_cd`.
  Weights are title `A`, tags/type/description `B`, body `D`. `<cfg>` is the
  configured `default_text_search_config` (default `pg_catalog.english`), the same
  configuration that built each `body_tsv` at index time — see the retroactivity
  warning below.
- Only **enabled** bundles are searched (`pgokf.bundles.enabled`).
- Each hit carries a `ts_headline` snippet over title, description, and body,
  computed with the same configured text-search configuration.
- Rows are ordered by descending rank, then ascending `concept_id` as a stable
  tiebreaker. Ranks are comparable **only within one query** — order by them,
  never persist them.

> **⚠️ `default_text_search_config` is applied but not retroactive.** The query
> is parsed under the current `default_text_search_config`, while each row's
> `body_tsv` was built under whatever configuration was in effect when that file
> was last indexed. `refresh_bundle` re-parses only files whose content hash
> changed, so changing the configuration leaves unchanged rows with stale
> vectors and search can return wrong or empty results for them. Set the
> configuration before the first `register_bundle`, or re-register a bundle
> (`unregister_bundle` + `register_bundle`) to rebuild its vectors under the new
> configuration. See [Configuration](configuration.md).

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

### `pgokf.find_similar(concept_id text, bundle_id bigint DEFAULT NULL, limit_count int DEFAULT 10) → SETOF pgokf.concept_search_result`

Content "more-like-this": rank the concepts whose body content is most similar to
a seed concept. `STABLE PARALLEL SAFE`, invoker rights, **requires
`pgokf_reader`**. This is distinct from `concept_neighbors`, which walks the
authored link graph — `find_similar` looks at what a concept *says*, not what it
*links to*.

| Parameter | Type | Default | Meaning |
| --------- | ---- | ------- | ------- |
| `concept_id` | `text` | — | The seed concept's id. If it exists in more than one bundle and `bundle_id` is `NULL`, the call raises `22023`; pass `bundle_id` to disambiguate. |
| `bundle_id` | `bigint` | `NULL` | Bundle scope for the seed. |
| `limit_count` | `int` | `10` | Maximum similar concepts; must be in `1..=500`. |

It extracts the seed's most salient `body_tsv` lexemes (highest term
frequencies), runs them as an `OR` query through the configured `search_backend`
(native FTS or BM25), and excludes the seed itself. Results are
`concept_search_result` rows ordered by relevance.

```sql
SELECT concept_id, round(rank::numeric, 4) AS rank
FROM pgokf.find_similar('runbooks/database-failover');
```

---

## Semantic and hybrid search (optional, pgvector)

These surfaces rank by **embedding similarity** and require the external
[`pgvector`](https://github.com/pgvector/pgvector) extension. Like the optional
BM25 backend, `pgokf` takes **no static dependency** on it: `CREATE EXTENSION
pgokf` succeeds without pgvector, embeddings are stored as the builtin `real[]`
in `pgokf.concept_embedding`, and the `vector` type is used only at query and
index time. `pgokf` never computes embeddings — a companion embedder streams
caller-computed vectors in via `set_concept_embedding` (see
[search-guide.md](search-guide.md)).

### `pgokf.set_concept_embedding(bundle_id bigint, concept_id text, embedding real[]) → void`

Store or replace one concept's embedding. `STRICT`, `SECURITY DEFINER`,
**requires `pgokf_writer`**. Validates that the concept exists and that
`length(embedding)` equals the durable `embedding_dim` config key (`22023`
otherwise), then upserts into `pgokf.concept_embedding`.

```sql
SELECT pgokf.set_concept_embedding(1, 'runbooks/database-failover',
                                   ARRAY[0.0123, -0.0456, ...]::real[]);
```

### `pgokf.concept_search_semantic(query_embedding real[], bundle_id bigint DEFAULT NULL, limit_count int DEFAULT 10) → SETOF pgokf.concept_search_result`

Nearest-neighbor search by pgvector cosine distance (`<=>`). `STABLE PARALLEL
SAFE`, invoker rights, **requires `pgokf_reader`**. The `rank` column is the
normalized cosine similarity (`1 - distance`, `1.0` for an identical vector); the
`headline` column is `NULL`. `query_embedding` must have `embedding_dim`
dimensions.

**Requires pgvector.** Because semantic search has no lexical equivalent, when
pgvector is not installed this raises `22023` naming the missing dependency
(`CREATE EXTENSION vector`) rather than silently returning nothing. Only enabled
bundles are searched.

```sql
SELECT concept_id, round(rank::numeric, 4) AS cosine_similarity
FROM pgokf.concept_search_semantic(ARRAY[0.0123, -0.0456, ...]::real[]);
```

### `pgokf.concept_search_hybrid(query text, query_embedding real[], bundle_id bigint DEFAULT NULL, limit_count int DEFAULT 10) → SETOF pgokf.concept_search_result`

Fuse the **lexical** result of `query` (through the configured `search_backend`)
with the **semantic** result of `query_embedding` using **Reciprocal Rank
Fusion** (RRF, k = 60), entirely in SQL. `STABLE PARALLEL SAFE`, invoker rights,
**requires `pgokf_reader`**. The `rank` column is the fused RRF score; a concept
strong in *both* lists outranks one strong in only one. When pgvector is not
installed, hybrid **degrades to lexical-only** with a `WARNING` (RRF needs no
model, so this fallback is sensible — unlike pure semantic search).

```sql
SELECT concept_id, round(rank::numeric, 6) AS rrf
FROM pgokf.concept_search_hybrid('database failover',
                                 ARRAY[0.0123, -0.0456, ...]::real[]);
```

### `pgokf.rebuild_embedding_index() → boolean`

(Re)build the pgvector HNSW cosine index on `pgokf.concept_embedding` for the
configured `embedding_dim`. `STRICT`, `SECURITY DEFINER`, **requires
`pgokf_admin`**. Mirrors `rebuild_search_index`: returns `true` when built, or
`false` (with a `NOTICE`) when pgvector is absent or `embedding_dim` exceeds
pgvector's 2000-dimension HNSW limit (semantic search then uses an exact scan).
Run it after enabling pgvector, after bulk-loading embeddings, or after changing
`embedding_dim`.

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
[`examples/queries/graph.sql`](https://github.com/LogicOcean/pgokf/blob/main/examples/queries/graph.sql).

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
--     "notify_channel": "",
--     "default_strict": true,
--     "default_exclude": [],
--     "search_backend": "native",
--     "okf_version_policy": "warn",
--     "embedding_dim": 1536,
--     "sync_log_retention_days": 30,
--     "default_text_search_config": "pg_catalog.english"
-- }
```

---

## Monitoring and audit

### `pgokf.list_sync_log(bundle_id bigint DEFAULT NULL, max_rows int DEFAULT 100) → SETOF pgokf.sync_log_entry`

List recent catalog sync/audit-log entries, newest first. `VOLATILE`,
`SECURITY DEFINER`, **requires `pgokf_reader`**.

Every successful `register` / `refresh` / `content` sync and every `unregister`
appends exactly one row to the administrator-only `pgokf_private.sync_log`,
inside the operation's own transaction (so a logged row always means the
operation committed). This function is the reader-facing projection over that
log. Pass `bundle_id` to scope the listing to one bundle; `max_rows` bounds the
number of rows (must be `>= 0`, else `22023`).

```sql
SELECT id, op, actor, added, updated, removed, total
FROM pgokf.list_sync_log();
--  id |    op    |  actor   | added | updated | removed | total
-- ----+----------+----------+-------+---------+---------+-------
--   2 | refresh  | app_sync |     0 |       0 |       0 |     4
--   1 | register | app_sync |     4 |       0 |       0 |     4
```

History is pruned to the `sync_log_retention_days` policy after each append; see
[configuration.md](configuration.md).

### `pgokf.catalog_stats() → SETOF pgokf.catalog_stat`

Per-bundle operational statistics for monitoring. `STABLE`, `PARALLEL SAFE`,
invoker rights, **requires `pgokf_reader`**.

One row per registered bundle with its indexed-concept, link, and resolved-link
counts, sync recency (`last_synced_at`, `sync_age`), and an `is_stale` flag
(true when the last sync is more than 24 hours old).

```sql
SELECT bundle_id, enabled, indexed_concepts, link_count, resolved_link_count,
       sync_age, is_stale
FROM pgokf.catalog_stats();
```

### `pgokf.health() → jsonb`

A single `jsonb` health document for liveness/readiness probes. `STABLE`,
`SECURITY DEFINER`, **requires `pgokf_reader`**.

```sql
SELECT jsonb_pretty(pgokf.health());
-- {
--     "ok": true,
--     "roles_ok": true,
--     "config_ok": true,
--     "bm25_ready": false,
--     "in_recovery": false,
--     "bundle_count": 1,
--     "concept_count": 4,
--     "search_backend": "native"
-- }
```

`ok` is `roles_ok AND config_ok`; `in_recovery` (`pg_is_in_recovery()`) supports
replica/readiness routing; `bm25_ready` reports whether the ParadeDB `pg_search`
extension and a `bm25` index on `pgokf.concepts` are both present.

### `pgokf.stale_concepts(bundle_id bigint DEFAULT NULL, as_of timestamptz DEFAULT NULL) → SETOF pgokf.stale_concept`

List concepts whose OKF `stale_after` instant has passed. `STABLE`,
`PARALLEL SAFE`, invoker rights, **requires `pgokf_reader`**.

Returns concepts whose `concept_provenance.stale_after` is earlier than `as_of`
(or `now()` when `as_of` is `NULL`), optionally scoped to one `bundle_id`.

```sql
-- Concepts already stale as of now:
SELECT bundle_id, concept_id, path, stale_after FROM pgokf.stale_concepts();
-- Concepts that will be stale by year end:
SELECT concept_id, stale_after
FROM pgokf.stale_concepts(NULL, '2026-12-31T23:59:59Z'::timestamptz);
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

## Source retrieval

These functions are only useful when the bundle was synced with the
`store_source` policy enabled, so `pgokf.concept_source` holds the verbatim
source bytes. See [configuration.md](configuration.md)
for the two-tier model. With `store_source` off (the default) no source is
stored, and `get_concept_source` raises `22023`.

### `pgokf.get_concept_source(bundle_id bigint, concept_id text) → bytea`

Return the exact stored source bytes of one concept, delivered to the client
(no filesystem write). `STABLE STRICT`, **reader-level** (`pgokf_reader` or
`pgokf_admin`). This discloses the same content as the concept's `body_text`, so
it carries no privilege beyond read access to the catalog.

| Parameter | Type | Meaning |
| --------- | ---- | ------- |
| `bundle_id` | `bigint` | The concept's bundle. |
| `concept_id` | `text` | The path-derived concept ID (see `pgokf.concepts.id`). |

Raises `22023` when the concept exists but no source was stored (the bundle was
synced with `store_source` disabled) and, distinctly in the message, when no such
concept exists.

```sql
-- Byte-for-byte identical to the original file on disk.
SELECT octet_length(pgokf.get_concept_source(1, 'alpha')) AS bytes;
SELECT convert_from(pgokf.get_concept_source(1, 'alpha'), 'UTF8');  -- as text
```

### `pgokf.export_sources(bundle_id bigint, dest_dir text) → pgokf.export_result`

Reconstruct a bundle's stored source files on the server filesystem, recreating
the bundle-relative directory tree under `dest_dir` and writing each concept's
verbatim bytes to `dest_dir/<concept path>`. `VOLATILE STRICT`,
`SECURITY DEFINER`, **requires `pgokf_admin`** — like `export_parquet`, it
**writes files** from inside the server process. See
[security.md](security.md#source-retrieval-and-reconstruction) for the threat
model.

| Parameter | Type | Meaning |
| --------- | ---- | ------- |
| `bundle_id` | `bigint` | The bundle to reconstruct; an unknown id raises `22023`. |
| `dest_dir` | `text` | Target directory, validated exactly like `export_parquet`'s: absolute, NUL-free, traversal-free, canonical, contained within `pgokf.allowed_roots` when configured, existing, and writable. Files are created with `O_NOFOLLOW` so a planted symlink cannot redirect a write. |

Behavior:

- Streams `pgokf.concept_source` joined to `pgokf.concepts.path` in bounded
  keyset batches, so peak memory is one batch regardless of bundle size.
- Verifies every written file against the concept's recorded BLAKE3 `file_hash`
  before writing it and raises `XX000` on any mismatch (a corrupted stored
  source — an integrity condition, not caller input), so a reconstruction is
  either byte-for-byte faithful or it fails without writing.
- Returns a `pgokf.export_result` in which `concepts_rows` is the number of files
  reconstructed and `bytes_written` their total size; the other per-table
  counters are `0` (this call reconstructs sources, not the four Parquet tables).
- Raises `42501` for a directory the server process cannot write.

```sql
SELECT concepts_rows AS files, bytes_written, dest_dir
FROM pgokf.export_sources(1, '/srv/okf-rebuild/sample');
--  files | bytes_written |        dest_dir
-- -------+---------------+-------------------------
--      4 |         14330 | /srv/okf-rebuild/sample
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
| `okf_version` | `text` | Bundle OKF version, from the reserved root `index.md` `okf_version` frontmatter (may be `NULL` when unset). |
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

### `pgokf.sync_log_entry`

Returned by `list_sync_log`.

| Column | Type | Meaning |
| ------ | ---- | ------- |
| `id` | `bigint` | Audit-entry identity. |
| `bundle_id` | `bigint` | Affected bundle (retained for `unregister` rows). |
| `bundle_path` | `text` | Bundle path captured at operation time. |
| `op` | `text` | `register` / `refresh` / `content` / `unregister`. |
| `actor` | `text` | The `session_user` that ran the operation. |
| `synced_at` | `timestamptz` | When the operation committed. |
| `added` / `updated` / `removed` / `unchanged` / `total` | `integer` | Per-bucket change counts (`NULL` for an `unregister`). |

### `pgokf.catalog_stat`

Returned by `catalog_stats`.

| Column | Type | Meaning |
| ------ | ---- | ------- |
| `bundle_id` | `bigint` | Bundle identity. |
| `name` | `text` | Optional label (may be `NULL`). |
| `enabled` | `boolean` | Whether the bundle is searched. |
| `source_type` | `text` | `filesystem` or `content`. |
| `file_count` | `integer` | Concept count recorded on the bundle row. |
| `indexed_concepts` | `bigint` | Live count of `pgokf.concepts` rows. |
| `link_count` | `bigint` | Total `pgokf.links` rows. |
| `resolved_link_count` | `bigint` | Resolved internal edges. |
| `last_synced_at` | `timestamptz` | Last successful sync (may be `NULL`). |
| `sync_age` | `interval` | `now() - last_synced_at` (may be `NULL`). |
| `is_stale` | `boolean` | True when the last sync is more than 24 hours old. |

### `pgokf.stale_concept`

Returned by `stale_concepts`.

| Column | Type | Meaning |
| ------ | ---- | ------- |
| `bundle_id` | `bigint` | Owning bundle. |
| `concept_id` | `text` | The stale concept's ID. |
| `path` | `text` | Bundle-relative source path. |
| `concept_type` | `text` | OKF concept type (may be `NULL`). |
| `stale_after` | `timestamptz` | The instant after which the concept is stale. |

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
| `path` | `text` | `NOT NULL`, `UNIQUE` — the canonical filesystem path for a filesystem bundle, or the synthetic key `content:<name>` for a content bundle. |
| `source_type` | `text` | `NOT NULL DEFAULT 'filesystem'`, `CHECK (source_type IN ('filesystem','content'))` — how bytes reach the catalog: `filesystem` (`register_bundle` / `refresh_bundle`) or `content` (`register_bundle_content`). |
| `name` | `text` | Optional label. |
| `okf_version` | `text` | The bundle's declared OKF version, read from the reserved bundle-root `index.md` `okf_version` frontmatter (e.g. `0.2`). `NULL` when the bundle has no root `index.md` or it declares no `okf_version`. |
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

Sparse scalar projection of OKF v0.2 provenance / trust / lifecycle frontmatter.
Only concepts that carry such frontmatter get a row. The `verified[]` events and
the `sources[]` materials live in the two child tables below.

| Column | Type | Notes |
| ------ | ---- | ----- |
| `bundle_id` | `bigint` | `NOT NULL`, part of FK to `pgokf.concepts`. |
| `concept_id` | `text` | `NOT NULL`, part of FK to `pgokf.concepts`. |
| `generated_by` | `text` | OKF `generated.by` (tolerates a bare `generated_by`) — the actor that produced the current content. `NULL` when absent. |
| `generated_at` | `timestamptz` | OKF `generated.at`, ISO 8601 (tolerates a bare `generated_at`). `NULL` when absent/unparseable (raw value kept in `details`). |
| `status` | `text` | OKF lifecycle `status` (`draft`/`stable`/`deprecated`). `NULL` when absent; the spec default for an absent status is `stable`. |
| `stale_after` | `timestamptz` | OKF `stale_after` — the absolute ISO 8601 instant after which content is stale. `NULL` when absent/unparseable. |
| `usage_window_from` | `timestamptz` | Top-level `usage_window.from` framing all source usage counts. `NULL` when absent/unparseable. |
| `usage_window_to` | `timestamptz` | Top-level `usage_window.to`. `NULL` when absent/unparseable. |
| `trust_tier` | `text` | **Derived**: `human-reviewed` if any `verified[]` actor is a `human:`, else `machine-confirmed` with ≥1 event, else `unverified`. |
| `details` | `jsonb` | `NOT NULL DEFAULT '{}'` — lossless copy of the recognized provenance/trust/lifecycle keys (`generated`, `verified`, `sources`, `usage_window`, `stale_after`, `status`, and the `generated_by`/`generated_at` aliases). |

Primary key `(bundle_id, concept_id)`; FK to `pgokf.concepts` `ON DELETE
CASCADE`. Index on `trust_tier`.

```sql
SELECT concept_id, generated_by, generated_at, status, stale_after, trust_tier
FROM pgokf.concept_provenance
ORDER BY concept_id;
```

### `pgokf.concept_verification`

The ordered OKF v0.2 `verified[]` event list — one row per verification event.
`verified` is a list of `{by, at}` mappings; a single mapping is stored as one
`ordinal = 0` row. Events with no actor are skipped (never stored as `NULL`).

| Column | Type | Notes |
| ------ | ---- | ----- |
| `bundle_id` | `bigint` | `NOT NULL`, part of FK to `pgokf.concepts`. |
| `concept_id` | `text` | `NOT NULL`, part of FK to `pgokf.concepts`. |
| `ordinal` | `integer` | `NOT NULL` — zero-based position in the `verified[]` list. |
| `verified_by` | `text` | `NOT NULL` — OKF `verified[].by`, the verifying actor (`<producer>/<version>`, `human:<id>`, or `process:<id>`). |
| `verified_at` | `timestamptz` | OKF `verified[].at`, ISO 8601. `NULL` when absent/unparseable. |

Primary key `(bundle_id, concept_id, ordinal)`; FK `(bundle_id, concept_id)` to
`pgokf.concepts(bundle_id, id)` `ON DELETE CASCADE`.

```sql
SELECT concept_id, ordinal, verified_by, verified_at
FROM pgokf.concept_verification
WHERE bundle_id = 1 ORDER BY concept_id, ordinal;
```

### `pgokf.concept_provenance_source`

The OKF v0.2 `sources[]` provenance materials — one row per source entry, the
inputs the content was derived from. Distinct from `pgokf.concept_source`, which
holds the concept's own raw source bytes. Non-object entries are skipped.

| Column | Type | Notes |
| ------ | ---- | ----- |
| `bundle_id` | `bigint` | `NOT NULL`, part of FK to `pgokf.concepts`. |
| `concept_id` | `text` | `NOT NULL`, part of FK to `pgokf.concepts`. |
| `ordinal` | `integer` | `NOT NULL` — zero-based position in the `sources[]` list. |
| `source_id` | `text` | OKF `sources[].id` — optional producer-defined identifier. |
| `resource` | `text` | OKF `sources[].resource` — the source URI. Spec-required per entry, stored leniently (`NULL` when absent) so a malformed source never aborts a sync. |
| `title` | `text` | OKF `sources[].title` — optional human-readable title. |
| `author` | `text` | OKF `sources[].author` — the actor credited with the source. |
| `usage_count` | `bigint` | OKF `sources[].usage_count` — uses within the usage window. `NULL` when absent/non-numeric. |
| `last_modified` | `timestamptz` | OKF `sources[].last_modified`, ISO 8601. `NULL` when absent/unparseable. |
| `usage_window_from` | `timestamptz` | Per-source `usage_window.from`, overriding the top-level window. `NULL` when absent. |
| `usage_window_to` | `timestamptz` | Per-source `usage_window.to`. `NULL` when absent. |

Primary key `(bundle_id, concept_id, ordinal)`; FK `(bundle_id, concept_id)` to
`pgokf.concepts(bundle_id, id)` `ON DELETE CASCADE`.

```sql
SELECT concept_id, ordinal, source_id, resource, author, usage_count
FROM pgokf.concept_provenance_source
WHERE bundle_id = 1 ORDER BY concept_id, ordinal;
```

### `pgokf.concept_source`

Opt-in verbatim source bytes of each concept file. Populated **only** when the
bundle was synced with the `store_source` policy enabled (the small,
self-contained tier); empty otherwise. Reader-`SELECT`able.

| Column | Type | Notes |
| ------ | ---- | ----- |
| `bundle_id` | `bigint` | `NOT NULL`, part of FK to `pgokf.concepts`. |
| `concept_id` | `text` | `NOT NULL`, part of FK to `pgokf.concepts`. |
| `raw_content` | `bytea` | `NOT NULL` — the exact, unmodified source-file bytes; hashes to `pgokf.concepts.file_hash` (BLAKE3). TOAST-compressed with `lz4` where the build supports it, otherwise `pglz`. |
| `byte_size` | `integer` | `NOT NULL` — length of `raw_content`, so a reader can size a retrieval without detoasting. |

Primary key `(bundle_id, concept_id)`; FK to `pgokf.concepts` `ON DELETE
CASCADE`, so removing a concept or unregistering a bundle drops the stored source
automatically. Retrieve bytes with `pgokf.get_concept_source`; reconstruct the
bundle on disk with `pgokf.export_sources`.

```sql
SELECT concept_id, byte_size FROM pgokf.concept_source
WHERE bundle_id = 1 ORDER BY concept_id;
```

### `pgokf.concept_embedding`

Opt-in per-concept embedding vectors, populated by `pgokf.set_concept_embedding`.
Reader-`SELECT`able. The vector is stored as the builtin **`real[]`** — never a
pgvector `vector` column — so `CREATE EXTENSION pgokf` succeeds without pgvector;
it is cast to `vector(dim)` at query and index time only when pgvector is
present.

| Column | Type | Notes |
| ------ | ---- | ----- |
| `bundle_id` | `bigint` | `NOT NULL`, part of FK to `pgokf.concepts`. |
| `concept_id` | `text` | `NOT NULL`, part of FK to `pgokf.concepts`. |
| `embedding` | `real[]` | `NOT NULL` — the caller-computed vector; length must equal `embedding_dim` at ingest. |
| `dim` | `integer` | `NOT NULL`, constrained equal to `cardinality(embedding)`. |
| `model` | `text` | Optional embedding-model/producer identifier for provenance. |
| `updated_at` | `timestamptz` | `NOT NULL DEFAULT now()` — when the row was last written. |

Primary key `(bundle_id, concept_id)`; FK to `pgokf.concepts` `ON DELETE
CASCADE`, so removing a concept or unregistering a bundle drops its embedding
automatically. Build the HNSW search index with `pgokf.rebuild_embedding_index`;
query with `pgokf.concept_search_semantic` / `pgokf.concept_search_hybrid`.

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
| `store_source` | `boolean` | `false` |
| `search_backend` | `text` | `'native'` (`CHECK IN ('native','bm25')`) |
| `notify_channel` | `text` | `''` (empty disables) |
| `okf_version_policy` | `text` | `'warn'` (`CHECK IN ('warn','reject')`) |
| `embedding_dim` | `integer` | `1536` (`CHECK BETWEEN 1 AND 16000`) |

### `pgokf_private.sync_log`

Administrator-only audit trail: one row per successful `register` / `refresh` /
`content` sync or bundle `unregister`, appended inside the operation's own
transaction and pruned to the `sync_log_retention_days` policy. No role has
direct access; read it through the reader-granted `pgokf.list_sync_log`
function.

| Column | Type | Notes |
| ------ | ---- | ----- |
| `id` | `bigint` | `GENERATED ALWAYS AS IDENTITY`, primary key. |
| `bundle_id` | `bigint` | Affected bundle (no FK, so `unregister` rows survive the delete). |
| `bundle_path` | `text` | Bundle path captured at operation time. |
| `op` | `text` | `NOT NULL`, `CHECK IN ('register','refresh','content','unregister')`. |
| `actor` | `text` | `NOT NULL DEFAULT session_user`. |
| `synced_at` | `timestamptz` | `NOT NULL DEFAULT now()`; the column retention prunes on. |
| `added` / `updated` / `removed` / `unchanged` / `total` | `integer` | Per-bucket change counts (`NULL` for an `unregister`). |
| `sync_hash` | `text` | Aggregate BLAKE3 digest of the synced snapshot (`NULL` for an `unregister`). |

---

## Roles

| Role | Login | Grants |
| ---- | ----- | ------ |
| `pgokf_reader` | `NOLOGIN` | `USAGE` on schema `pgokf`; `SELECT` on the projection tables (including `concept_source`); `EXECUTE` on search/graph/list/`get_config`/`get_concept_source`. |
| `pgokf_admin` | `NOLOGIN` | Everything `pgokf_reader` has (it is `GRANT`ed `pgokf_reader`), plus `USAGE` on `pgokf_private` and `EXECUTE` on `register_bundle`, `refresh_bundle`, `unregister_bundle`, `set_config`, `reset_config`, `export_parquet`, `export_sources`. |

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
