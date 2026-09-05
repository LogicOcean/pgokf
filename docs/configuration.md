# pgokf configuration

`pgokf` has two distinct configuration surfaces, and the split is deliberate:

1. **GUCs** (`pgokf.*` server settings) - hard, per-cluster **safety ceilings**.
   They are set only in `postgresql.conf` and cannot be raised from a SQL
   session, so they stay trustworthy as limits.
2. **Durable policy** (`pgokf_private.config`) - catalog **policy** an
   administrator manages through SQL. It persists across restarts and is edited
   with `pgokf.set_config` / `pgokf.reset_config` / `pgokf.get_config`.

Everything below is taken from `crates/extension/src/guc.rs` and
`crates/extension/src/catalog/config.rs` and was verified against a live cluster.

---

## GUCs (resource ceilings)

Registered in `_PG_init`. The four numeric ceilings use the **`SIGHUP`**
context: they change only via `postgresql.conf` plus a configuration reload
(`SELECT pg_reload_conf();` or `pg_ctl reload`), and **no session - not even a
superuser `SET`** - can raise them. That is what makes them dependable hard
limits rather than advisory defaults. (`PGC_POSTMASTER` would be stricter but
PostgreSQL forbids defining such variables for a library loaded on demand by
`CREATE EXTENSION`, so `SIGHUP` is the strictest usable context.) The logging
threshold uses **`SUSET`**, so a superuser can adjust it at runtime.

| GUC | Type | Default | Range | Context | Effect |
| --- | ---- | ------- | ----- | ------- | ------ |
| `pgokf.max_file_bytes` | integer | `4194304` (4 MiB) | `1 .. 2147483647` | `SIGHUP` | Maximum bytes read from one bundle file; larger files abort the sync. |
| `pgokf.max_bundle_files` | integer | `100000` | `1 .. 2147483647` | `SIGHUP` | Maximum files discovered in one bundle. |
| `pgokf.max_frontmatter_bytes` | integer | `262144` (256 KiB) | `1 .. 2147483647` | `SIGHUP` | Maximum bytes parsed as YAML frontmatter in one document. |
| `pgokf.max_graph_hops` | integer | `5` | `1 .. 1000` | `SIGHUP` | Hard ceiling for graph traversal depth; `concept_neighbors(max_hops)` is capped to this. |
| `pgokf.log_level` | string | `warning` | - | `SUSET` | Logging threshold used by `pgokf`. |
| `pgokf.tenant` | string | `''` (empty = see all) | - | `USERSET` | Active tenant for multi-tenant row-level isolation. A policy selector, not a ceiling: any session may set it. |

### Reading and setting GUCs

```sql
SHOW pgokf.max_graph_hops;   -- read the effective value
```

To change a ceiling, add it to `postgresql.conf` and reload:

```conf
# postgresql.conf
pgokf.max_file_bytes = 8388608      # 8 MiB
pgokf.max_bundle_files = 250000
pgokf.max_graph_hops = 8
```

```sql
SELECT pg_reload_conf();
```

Attempting to raise a `SIGHUP` ceiling from SQL fails - this is by design:

```sql
SET pgokf.max_graph_hops = 50;
-- ERROR:  parameter "pgokf.max_graph_hops" cannot be set after connection start
```

`max_graph_hops` interacts with `concept_neighbors`: a `max_hops` argument above
the ceiling is silently capped to it, and `max_hops < 1` is rejected with
`22023`.

### Tenant selector - `pgokf.tenant`

Unlike the ceilings above, `pgokf.tenant` is a `USERSET` policy selector, not a
safety limit. It chooses the tenant a session reads and writes under for
multi-tenant row-level isolation. Its empty default preserves the
pre-multi-tenancy see-all behavior, so an install that never sets it is
unaffected. Set it per session, per role, or per connection:

```sql
SET pgokf.tenant = 'acme';                    -- this session
ALTER ROLE acme_app SET pgokf.tenant = 'acme'; -- every connection as this role
RESET pgokf.tenant;                            -- back to see-all
```

See [multi-tenancy.md](multi-tenancy.md) for the full model, including the
strict-isolation contract (pin the tenant, connect as a non-superuser reader).

---

## Durable policy (`pgokf_private.config`)

A single, cluster-persistent policy row in the administrator-only
`pgokf_private` schema, with one typed column per setting and a boolean-singleton
primary key so exactly one row can ever exist. No role has direct DML; every read
and write flows through the `SECURITY DEFINER` config functions, which authorize
the caller first (`get_config` is reader-level; `set_config`/`reset_config` are
admin-only).

| Key | JSON shape (for `set_config`) | Default | Validation |
| --- | ----------------------------- | ------- | ---------- |
| `allowed_roots` | array of strings | `[]` | each entry absolute and traversal-free |
| `default_text_search_config` | string | `"pg_catalog.english"` | non-empty; must name a config in `pg_catalog.pg_ts_config` |
| `default_strict` | boolean | `true` | must be a boolean |
| `sync_log_retention_days` | integer | `30` | `>= 0` and fits `integer` |
| `default_exclude` | array of strings | `[]` | each pattern non-empty and NUL-free |
| `store_source` | boolean | `false` | must be a boolean |
| `search_backend` | string | `"native"` | one of `"native"`, `"bm25"` |
| `bm25_provider` | string | `"auto"` | one of `"auto"`, `"pg_textsearch"`, `"pg_search"` |
| `notify_channel` | string | `""` | empty (disabled) or a safe channel identifier (letters, digits, underscore; leading letter/underscore; ≤ 63 bytes) |
| `okf_version_policy` | string | `"warn"` | one of `"warn"`, `"reject"` |
| `embedding_dim` | integer | `1536` | between `1` and `16000` |
| `track_history` | boolean | `false` | must be a boolean |
| `history_retention_days` | integer | `0` | `>= 0` and fits `integer` |

### Which keys the current engine consults

Accuracy matters here. As of this release the engine consults **every** key:
it **enforces `allowed_roots`**, **applies `default_text_search_config`** to
both indexing and querying, **honors `store_source`**, `search_backend`,
`bm25_provider`, `default_strict`, and `default_exclude`, and **activates
`sync_log_retention_days`**, `notify_channel`, `okf_version_policy`,
`embedding_dim`, `track_history`, and `history_retention_days`:

| Key | Status in the current engine |
| --- | ---------------------------- |
| `allowed_roots` | **Enforced.** When non-empty, a registered bundle path must resolve inside one of the roots (symlink-escape safe), else `22023`. |
| `default_text_search_config` | **Applied.** Used as the `regconfig` for `to_tsvector` when building each concept's `body_tsv` at index time, and for `websearch_to_tsquery`/`ts_headline` at query time, so query parsing matches the configuration that indexed the rows. See the caveat below. |
| `store_source` | **Honored.** When `true`, sync stores each concept's verbatim source bytes in `pgokf.concept_source`; when `false` (default) it stores none. See the storage-tiers section below. |
| `search_backend` | **Applied.** Selects the ranked-search backend `pgokf.concept_search` dispatches to: `native` PostgreSQL FTS (default) or `bm25` (an external BM25 provider extension). When `bm25` but the provider or its index is absent, search falls back to native with a warning. See the search-backend section below. |
| `bm25_provider` | **Applied (new in 0.1.15).** Which BM25 provider the `bm25` backend runs on: `auto` (default: Tiger Data `pg_textsearch` when installed, else ParadeDB `pg_search`), or one of the two by name. `rebuild_search_index()` builds the resolved provider's index. See the search-backend section below. |
| `sync_log_retention_days` | **Applied (new in 0.1.5).** After each successful sync appends its `pgokf_private.sync_log` audit row, history older than `now() - this many days` is pruned in the same transaction. `0` (or no older rows) keeps history indefinitely. See the audit-log section below. |
| `notify_channel` | **Applied (new in 0.1.5).** When non-empty, a successful sync emits `pg_notify(<channel>, <json>)`; empty (default) disables it with zero overhead. See the change-notification section below. |
| `okf_version_policy` | **Applied (new in 0.1.5).** Governs how sync treats a bundle that declares an unsupported OKF `okf_version`: `warn` (default) logs a `WARNING` and indexes anyway, `reject` aborts with `22023`. See the version-policy section below. |
| `embedding_dim` | **Applied (new in 0.1.6).** The expected length of caller-supplied embeddings: `pgokf.set_concept_embedding` rejects any `real[]` whose length differs, and `pgokf.rebuild_embedding_index` builds its pgvector HNSW index with the `vector(embedding_dim)` typmod. See the embeddings section below. |
| `track_history` | **Applied (new in 0.1.11).** When `true`, each sync records an SCD-2 version trail of every changed concept into `pgokf.concept_history` (read via `pgokf.concept_history` / `pgokf.concept_as_of`); when `false` (default) it records nothing, with zero storage/behavior change. See the version-history section below and [Version History](version-history.md). |
| `history_retention_days` | **Applied (new in 0.1.11).** When `track_history` is on and this is positive, closed history versions older than `now() - this many days` are pruned in the same transaction after each sync; the current open version is never pruned. `0` (default) keeps history indefinitely. See the version-history section below. |
| `default_strict` | **Applied.** When `true` (default), the first malformed concept file aborts the sync with `22023` and the surrounding transaction rolls back, so a partial projection is never committed. When `false`, a malformed file is logged as a `WARNING` and passed over (it counts toward no bucket in the returned report), so the rest of the bundle still registers. A file that cannot be *read* (an I/O failure, not a parse failure) remains a hard error in both modes. |
| `default_exclude` | **Applied.** Bundle-relative glob patterns excluded from filesystem discovery at sync time, combined with the built-in exclusions. Content-sourced bundles (`register_bundle_content`) have no filesystem discovery, so the globs do not apply to them. |

### Storage tiers - `store_source`

`store_source` selects between two deployment shapes without changing any other
behavior. It is **off by default**, so an install that never touches it behaves
exactly as one built without the feature.

- **Small, self-contained (`store_source = true`).** Sync retains the source
  bytes it already read to parse each concept and persists them into
  `pgokf.concept_source`, so the *original* files live inside PostgreSQL. Such an
  install needs no external object store: the catalog is the source of truth, a
  reader can fetch a concept's exact bytes with
  `pgokf.get_concept_source(bundle_id, concept_id)`, and an admin can rebuild the
  bundle on disk byte-for-byte with
  `pgokf.export_sources(bundle_id, dest_dir)`. The cost is storage - every source
  file is held in the database (TOAST-compressed, `lz4` where the build supports
  it, otherwise `pglz`).
- **Enterprise data-lake (`store_source = false`, default).** The verbatim files
  stay in a mounted object store / data lake and PostgreSQL holds only the
  metadata-and-search projection. No `concept_source` row is written, so the
  database stays lean and the lake remains the system of record.

> **⚠️ Warning - changing `store_source` is not retroactive.**
> Like `default_text_search_config`, `store_source` is read at the moment each
> concept is synced. Turning it on does **not** backfill source bytes for bundles
> that are already synced, and `refresh_bundle` only re-reads files whose content
> hash changed. Set `store_source` **before the first `register_bundle`** so the
> whole corpus is stored under one policy; to add (or drop) stored source for an
> already-registered bundle, re-register it - `unregister_bundle` followed by
> `register_bundle`.

> **⚠️ Warning - changing `default_text_search_config` is not retroactive.**
> The configuration is read at the moment each concept is indexed and again at
> the moment each query runs. Changing it does **not** re-index bundles that are
> already synced: `refresh_bundle` only re-parses files whose content hash
> changed, so every unchanged row keeps the `body_tsv` that was built under the
> **previous** config. Search then parses the query under the **new** config and
> can mismatch those stale vectors, returning wrong or empty results for the
> affected bundles.
>
> Set `default_text_search_config` **before the first `register_bundle`** so the
> whole corpus is indexed under one configuration. To move an already-registered
> bundle to a new configuration, re-register it - `unregister_bundle` followed by
> `register_bundle` - which is currently the only way to rebuild every `body_tsv`
> under the new config. A `pg_textsearch` BM25 index bakes the configuration in
> at build time too, so after the change also run `rebuild_search_index()`.

### Search backend - `search_backend`

`search_backend` selects the strategy `pgokf.concept_search` runs behind its
fixed signature and result shape:

- **`native`** (the default) - zero-dependency PostgreSQL full-text search
  (`websearch_to_tsquery` + `ts_rank_cd` + `ts_headline` over the weighted
  `body_tsv` GIN index). It works on every supported server (PG 15–19) with no
  extra extension, and remains the right choice for selective queries.
- **`bm25`** - BM25 top-k over an external provider's `bm25` index, which is
  dramatically faster for broad, relevance-ranked queries. It requires one of
  two provider extensions (see below).

### BM25 provider - `bm25_provider`

`bm25_provider` (new in 0.1.15) selects which extension the `bm25` backend
runs on. Both register an index access method named `bm25`, so at most one
can be created in a database, and the resolution is unambiguous:

| Value | Provider | License | PostgreSQL |
| ----- | -------- | ------- | ---------- |
| `auto` (default) | `pg_textsearch` when installed, else `pg_search` | - | - |
| `pg_textsearch` | [Tiger Data `pg_textsearch`](https://github.com/timescale/pg_textsearch) | PostgreSQL license | 17, 18 |
| `pg_search` | [ParadeDB `pg_search`](https://github.com/paradedb/paradedb) | AGPL-3.0 (community edition) | 15-18 |

`pgokf` never links either provider at build time - `CREATE EXTENSION pgokf`
succeeds whether or not one is installed. When `search_backend` is `bm25` but
the resolved provider is not installed (or a named provider is absent), or no
`bm25` index exists on `pgokf.concepts`, `concept_search` **falls back to
native with a `WARNING`** instead of erroring. Build the index with the
admin-only `pgokf.rebuild_search_index()`, which builds the resolved
provider's index, and see [`search-guide.md`](search-guide.md) for the full
enable-BM25 walkthrough, the provider comparison (query syntax, tokenizers,
`shared_preload_libraries`, PG-version constraints), and the licensing note.

> **Honesty note - `bm25` needs an external extension.** `native` is the
> default precisely because it has no dependencies. `bm25` is opt-in and pulls
> in a provider extension that must be added to `shared_preload_libraries`
> (and, for `pg_search`, requires `pgvector` too). If you cannot or do not
> want that dependency, stay on `native`.

### Embedding dimension - `embedding_dim`

`embedding_dim` (default `1536`) is the expected length of the caller-computed
embedding vectors streamed in through `pgokf.set_concept_embedding`, and the
typmod (`vector(embedding_dim)`) that `pgokf.rebuild_embedding_index` builds its
pgvector HNSW index with.

```sql
-- match your embedding model (e.g. a 768-dim model)
SELECT pgokf.set_config('embedding_dim', '768'::jsonb);
```

`pgokf` **never computes embeddings** and takes **no static dependency on
pgvector** - the `pgokf.concept_embedding` table stores each vector as the
builtin `real[]`, so `CREATE EXTENSION pgokf` succeeds on a cluster without
pgvector. The optional semantic (`concept_search_semantic`) and hybrid
(`concept_search_hybrid`) surfaces cast that `real[]` to `vector(embedding_dim)`
only at query time, and only when pgvector is installed. See
[`search-guide.md`](search-guide.md) for the embedding-companion integration and
the full semantic/hybrid walkthrough.

> **Not retroactive.** Changing `embedding_dim` does not rewrite already-stored
> embeddings; re-ingest them at the new dimension and re-run
> `pgokf.rebuild_embedding_index()`. HNSW indexing applies up to pgvector's
> 2000-dimension index limit; above it semantic search still works via an exact
> scan.

### Audit-log retention - `sync_log_retention_days`

Every successful `register` / `refresh` / `register_bundle_content` sync, and
every `unregister_bundle`, appends one row to the administrator-only
`pgokf_private.sync_log` audit trail, inside the operation's own transaction (so
a logged row always means the operation committed). Immediately after appending,
history older than `now() - sync_log_retention_days` is **pruned in the same
transaction**. A value of `0` - or simply no rows older than the window - keeps
history indefinitely.

This is new in 0.1.5: the key was defined but dead in earlier releases. Read the
log through the reader-level `pgokf.list_sync_log(bundle_id, max_rows)` function
(see [sql-api.md](sql-api.md) and [operations.md](operations.md)).

As of 0.1.8 this one key governs **all three** audit trails, so operators tune a
single retention window: the sync log; the per-concept **change manifest**
(`pgokf_private.sync_log_change`, which cascades from `sync_log` and so is pruned
with it); and the exfiltration **access log** (`pgokf_private.access_log`, pruned
on the same policy after each `export_parquet` / `export_sources` /
`get_concept_source` append).

```sql
SELECT pgokf.set_config('sync_log_retention_days', '14'::jsonb);  -- keep 14 days
SELECT pgokf.set_config('sync_log_retention_days', '0'::jsonb);   -- keep forever
```

### Change notification - `notify_channel`

When `notify_channel` is a non-empty channel identifier, a successful sync emits
`pg_notify(<channel>, <json>)` with a payload of
`{bundle_id, op, added, updated, removed, total}`. A `LISTEN <channel>` client
(an external indexer, a cache invalidator, a dashboard) is then woken on commit.
Empty (the default) disables it entirely, with zero overhead - no `pg_notify`
call is made. The channel name is validated as a safe identifier and always
bound as a parameter, never interpolated.

```sql
SELECT pgokf.set_config('notify_channel', '"pgokf_events"'::jsonb);  -- enable
SELECT pgokf.set_config('notify_channel', '""'::jsonb);              -- disable
-- Consumer side:
LISTEN pgokf_events;
```

### OKF version policy - `okf_version_policy`

A bundle-root `index.md` may declare an `okf_version`. This build models OKF
v0.2, so it recognizes `0.2` (and its patch line `0.2.x`, lenient on a leading
`v`). `okf_version_policy` governs a bundle that declares an *unsupported*
version:

- **`warn`** (the default) - log a `WARNING` and index the bundle anyway; the
  declared version is still stored on `pgokf.bundles.okf_version`.
- **`reject`** - abort the sync with `22023`, so a non-conforming bundle never
  commits.

An **absent** `okf_version` is always accepted and leaves
`pgokf.bundles.okf_version` `NULL`, under either policy.

```sql
SELECT pgokf.set_config('okf_version_policy', '"reject"'::jsonb);  -- strict
SELECT pgokf.set_config('okf_version_policy', '"warn"'::jsonb);    -- lenient (default)
```

### Version history - `track_history` / `history_retention_days`

`track_history` is the **opt-in switch** for concept version history, and it is
**off by default**. When it is `false` (the default) a sync records nothing, so
an existing install - and any bundle synced with history disabled - behaves
**exactly as before with zero extra storage**. That is precisely what keeps the
feature backward compatible; treat enabling it as a **storage/retention
tradeoff**.

When `track_history` is `true`, every register/refresh/content sync records an
append-only [SCD Type-2](version-history.md) version trail of each *changed*
concept into **`pgokf.concept_history`**, inside the same transaction (so history
commits atomically with the sync). Read it back with
`pgokf.concept_history(bundle_id, concept_id)` (the newest-first timeline) and
`pgokf.concept_as_of(bundle_id, concept_id, as_of)` (the point-in-time snapshot).

`history_retention_days` bounds growth: when positive, **closed** history versions
(`valid_to IS NOT NULL`) older than `now() - this many days` are pruned in the
same transaction after each sync. The single **current open** version of each
concept (`valid_to IS NULL`) is never pruned, so present-time point-in-time
queries always resolve. `0` (the default) keeps history indefinitely.

```sql
SELECT pgokf.set_config('track_history', 'true'::jsonb);            -- start recording
SELECT pgokf.set_config('history_retention_days', '90'::jsonb);     -- prune closed versions > 90 days
SELECT pgokf.set_config('track_history', 'false'::jsonb);           -- stop (existing history is kept)
```

> **⚠️ Not retroactive.** Enabling `track_history` starts recording at the *next*
> sync; concepts already present are not backfilled. A concept first versioned
> after history was enabled begins its chain at that sync's `change_kind` (its
> version 1). See [Version History](version-history.md) for the full model.

### Reading the effective policy

`get_config()` returns the whole row as a `jsonb` object (reader-level):

```sql
SELECT jsonb_pretty(pgokf.get_config());
-- {
--     "allowed_roots": [],
--     "notify_channel": "",
--     "store_source": false,
--     "track_history": false,
--     "search_backend": "native",
--     "bm25_provider": "auto",
--     "default_strict": true,
--     "default_exclude": [],
--     "okf_version_policy": "warn",
--     "embedding_dim": 1536,
--     "history_retention_days": 0,
--     "sync_log_retention_days": 30,
--     "default_text_search_config": "pg_catalog.english"
-- }

SELECT pgokf.get_config() -> 'allowed_roots';   -- read one key
```

### Setting a key

`set_config(key, value)` is polymorphic: one entry point, each key carrying its
natural `jsonb` shape. Values are validated and coerced per key; an unknown key
or a wrong-shaped/out-of-domain value raises `22023`.

```sql
-- Restrict registration to one or more roots (the recommended hardening step):
SELECT pgokf.set_config('allowed_roots', '["/srv/okf-bundles", "/data/knowledge"]'::jsonb);

-- Choose the text-search configuration (applied at index and query time).
-- Set this BEFORE the first register_bundle; see the retroactivity warning above.
SELECT pgokf.set_config('default_text_search_config', '"pg_catalog.simple"'::jsonb);

-- Opt into the small, self-contained tier: store each concept's verbatim source
-- bytes in pgokf.concept_source. Set this BEFORE the first register_bundle; see
-- the store_source retroactivity warning above.
SELECT pgokf.set_config('store_source', 'true'::jsonb);

-- Switch ranked search to BM25 (then run rebuild_search_index). Falls back to
-- native, with a warning, if the provider extension or its index is absent.
SELECT pgokf.set_config('search_backend', '"bm25"'::jsonb);

-- Pin the BM25 provider (new in 0.1.15); 'auto' prefers pg_textsearch.
SELECT pgokf.set_config('bm25_provider', '"pg_textsearch"'::jsonb);

-- Audit-log retention (new in 0.1.5): keep 14 days of sync history; 0 = forever.
SELECT pgokf.set_config('sync_log_retention_days', '14'::jsonb);

-- Change notification (new in 0.1.5): announce each sync on a LISTEN channel.
SELECT pgokf.set_config('notify_channel', '"pgokf_events"'::jsonb);

-- OKF version policy (new in 0.1.5): reject bundles that declare an unsupported
-- okf_version instead of warning.
SELECT pgokf.set_config('okf_version_policy', '"reject"'::jsonb);

-- Sync policy: warn-and-skip malformed files instead of aborting, and
-- exclude scratch paths from discovery.
SELECT pgokf.set_config('default_strict', 'false'::jsonb);
SELECT pgokf.set_config('default_exclude', '["*.tmp", "drafts/**"]'::jsonb);
```

Validation examples (all `22023`):

```sql
SELECT pgokf.set_config('nope', '1'::jsonb);
-- ERROR:  22023: unknown configuration key: nope
SELECT pgokf.set_config('allowed_roots', '["relative/dir"]'::jsonb);
-- ERROR:  22023: path must be absolute: relative/dir
SELECT pgokf.set_config('sync_log_retention_days', '-1'::jsonb);
-- ERROR:  22023: sync_log_retention_days must be greater than or equal to 0
SELECT pgokf.set_config('default_text_search_config', '"no_such_config"'::jsonb);
-- ERROR:  22023: text search configuration does not exist: no_such_config
SELECT pgokf.set_config('search_backend', '"solr"'::jsonb);
-- ERROR:  22023: search_backend must be one of 'native', 'bm25', got solr
SELECT pgokf.set_config('notify_channel', '"1 drop"'::jsonb);
-- ERROR:  22023: notify_channel must be a safe identifier ...
SELECT pgokf.set_config('okf_version_policy', '"ignore"'::jsonb);
-- ERROR:  22023: okf_version_policy must be one of 'warn', 'reject', got ignore
```

### Resetting keys

```sql
SELECT pgokf.reset_config('allowed_roots');  -- reset one key to its default
SELECT pgokf.reset_config();                 -- reset every key to its default
```

---

## GUCs vs. durable policy - when to use which

- Use **GUCs** for safety ceilings that operators, not the catalog, own: how big a
  file may be, how many files a bundle may hold, how deep a graph walk may go.
  They are cluster-wide, reload-only, and cannot be relaxed from SQL.
- Use **durable policy** for catalog behavior an administrator tunes through SQL
  and that must survive restarts: where bundles may live (`allowed_roots`), how
  search runs (`search_backend`, `bm25_provider`, `default_text_search_config`,
  `embedding_dim`),
  what the catalog stores (`store_source`, `track_history`), how sync reacts and
  announces (`okf_version_policy`, `notify_channel`), and how long audit and
  history are kept (`sync_log_retention_days`, `history_retention_days`).
