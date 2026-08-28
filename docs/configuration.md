# pgokf configuration

`pgokf` has two distinct configuration surfaces, and the split is deliberate:

1. **GUCs** (`pgokf.*` server settings) — hard, per-cluster **safety ceilings**.
   They are set only in `postgresql.conf` and cannot be raised from a SQL
   session, so they stay trustworthy as limits.
2. **Durable policy** (`pgokf_private.config`) — catalog **policy** an
   administrator manages through SQL. It persists across restarts and is edited
   with `pgokf.set_config` / `pgokf.reset_config` / `pgokf.get_config`.

Everything below is taken from `crates/extension/src/guc.rs` and
`crates/extension/src/catalog/config.rs` and was verified against a live cluster.

---

## GUCs (resource ceilings)

Registered in `_PG_init`. The four numeric ceilings use the **`SIGHUP`**
context: they change only via `postgresql.conf` plus a configuration reload
(`SELECT pg_reload_conf();` or `pg_ctl reload`), and **no session — not even a
superuser `SET`** — can raise them. That is what makes them dependable hard
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
| `pgokf.log_level` | string | `warning` | — | `SUSET` | Logging threshold used by `pgokf`. |

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

Attempting to raise a `SIGHUP` ceiling from SQL fails — this is by design:

```sql
SET pgokf.max_graph_hops = 50;
-- ERROR:  parameter "pgokf.max_graph_hops" cannot be set after connection start
```

`max_graph_hops` interacts with `concept_neighbors`: a `max_hops` argument above
the ceiling is silently capped to it, and `max_hops < 1` is rejected with
`22023`.

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

### Which keys the current engine consults

Accuracy matters here. As of this release the engine **enforces
`allowed_roots`**, **applies `default_text_search_config`** to both indexing and
querying, and **honors `store_source`** at sync time; the remaining three keys
are validated and durably stored but are **not yet consulted** by the
ingest/search paths:

| Key | Status in the current engine |
| --- | ---------------------------- |
| `allowed_roots` | **Enforced.** When non-empty, a registered bundle path must resolve inside one of the roots (symlink-escape safe), else `22023`. |
| `default_text_search_config` | **Applied.** Used as the `regconfig` for `to_tsvector` when building each concept's `body_tsv` at index time, and for `websearch_to_tsquery`/`ts_headline` at query time, so query parsing matches the configuration that indexed the rows. See the caveat below. |
| `store_source` | **Honored.** When `true`, sync stores each concept's verbatim source bytes in `pgokf.concept_source`; when `false` (default) it stores none. See the storage-tiers section below. |
| `search_backend` | **Applied.** Selects the ranked-search backend `pgokf.concept_search` dispatches to: `native` PostgreSQL FTS (default) or `bm25` (ParadeDB `pg_search`). When `bm25` but `pg_search` or its index is absent, search falls back to native with a warning. See the search-backend section below. |
| `default_strict` | **Stored, not yet consulted.** Sync is always strict — the first malformed file aborts the sync. |
| `sync_log_retention_days` | **Stored, not yet consulted.** No sync-log retention is wired up yet. |
| `default_exclude` | **Stored, not yet consulted.** Discovery does not yet apply these exclusion globs. |

The remaining three are reserved for planned functionality; setting them is safe
and persists, but does not change behavior today.

### Storage tiers — `store_source`

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
  `pgokf.export_sources(bundle_id, dest_dir)`. The cost is storage — every source
  file is held in the database (TOAST-compressed, `lz4` where the build supports
  it, otherwise `pglz`).
- **Enterprise data-lake (`store_source = false`, default).** The verbatim files
  stay in a mounted object store / data lake and PostgreSQL holds only the
  metadata-and-search projection. No `concept_source` row is written, so the
  database stays lean and the lake remains the system of record.

> **⚠️ Warning — changing `store_source` is not retroactive.**
> Like `default_text_search_config`, `store_source` is read at the moment each
> concept is synced. Turning it on does **not** backfill source bytes for bundles
> that are already synced, and `refresh_bundle` only re-reads files whose content
> hash changed. Set `store_source` **before the first `register_bundle`** so the
> whole corpus is stored under one policy; to add (or drop) stored source for an
> already-registered bundle, re-register it — `unregister_bundle` followed by
> `register_bundle`.

> **⚠️ Warning — changing `default_text_search_config` is not retroactive.**
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
> bundle to a new configuration, re-register it — `unregister_bundle` followed by
> `register_bundle` — which is currently the only way to rebuild every `body_tsv`
> under the new config.

### Search backend — `search_backend`

`search_backend` selects the strategy `pgokf.concept_search` runs behind its
fixed signature and result shape:

- **`native`** (the default) — zero-dependency PostgreSQL full-text search
  (`websearch_to_tsquery` + `ts_rank_cd` + `ts_headline` over the weighted
  `body_tsv` GIN index). It works on every supported server (PG 15–19) with no
  extra extension, and remains the right choice for selective queries.
- **`bm25`** — Block-Max WAND top-k over a ParadeDB `pg_search` index, which is
  dramatically faster for broad, relevance-ranked queries. It requires the
  external `pg_search` extension (see below).

`pgokf` never links `pg_search` at build time — `CREATE EXTENSION pgokf`
succeeds whether or not `pg_search` is installed. When `search_backend` is
`bm25` but `pg_search` is not installed, or no `bm25` index exists on
`pgokf.concepts`, `concept_search` **falls back to native with a `WARNING`**
instead of erroring. Build the index with the admin-only
`pgokf.rebuild_search_index()`, and see [`search-guide.md`](search-guide.md) for
the full enable-BM25 walkthrough, the honesty notes about the `pg_search`
dependency (AGPL-3.0, `shared_preload_libraries`, PG-version constraints), and
the tokenizer differences between the two backends.

> **Honesty note — `bm25` needs an external extension.** `native` is the
> default precisely because it has no dependencies. `bm25` is opt-in and pulls
> in ParadeDB `pg_search` (which itself requires `pgvector` and a
> `shared_preload_libraries` entry). If you cannot or do not want that
> dependency, stay on `native`.

### Reading the effective policy

`get_config()` returns the whole row as a `jsonb` object (reader-level):

```sql
SELECT jsonb_pretty(pgokf.get_config());
-- {
--     "allowed_roots": [],
--     "store_source": false,
--     "search_backend": "native",
--     "default_strict": true,
--     "default_exclude": [],
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

-- Switch ranked search to ParadeDB pg_search BM25 (then run rebuild_search_index).
-- Falls back to native, with a warning, if pg_search or its index is absent.
SELECT pgokf.set_config('search_backend', '"bm25"'::jsonb);

-- Reserved keys (stored, not yet consulted by the engine):
SELECT pgokf.set_config('default_strict', 'false'::jsonb);
SELECT pgokf.set_config('sync_log_retention_days', '14'::jsonb);
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
```

### Resetting keys

```sql
SELECT pgokf.reset_config('allowed_roots');  -- reset one key to its default
SELECT pgokf.reset_config();                 -- reset every key to its default
```

---

## GUCs vs. durable policy — when to use which

- Use **GUCs** for safety ceilings that operators, not the catalog, own: how big a
  file may be, how many files a bundle may hold, how deep a graph walk may go.
  They are cluster-wide, reload-only, and cannot be relaxed from SQL.
- Use **durable policy** for catalog behavior an administrator tunes through SQL
  and that must survive restarts — today, principally `allowed_roots` to bound
  where bundles may live.
