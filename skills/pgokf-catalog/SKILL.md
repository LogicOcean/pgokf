---
name: pgokf-catalog
description: Use when an agent needs to set up, register, search, traverse, or administer the pgokf OKF PostgreSQL catalog extension - installing it, granting the reader/writer/admin roles, registering and refreshing bundles, running lexical/semantic/hybrid concept_search, walking the link graph with concept_neighbors, reading provenance/metadata, retrieving raw source, tuning config/GUCs, or decoding a 22023/42501/23505 error.
---

# Using the OKF PostgreSQL catalog (pgokf)

`pgokf` is a PostgreSQL extension that catalogs Open Knowledge Format (OKF)
Markdown bundles. It discovers `.md` files under a bundle root, parses YAML
frontmatter + Markdown body + links, and projects them into tables with
full-text search (plus optional semantic and hybrid search), a link graph,
provenance, and opt-in raw-source storage.
Everything lives in schema `pgokf` (plus the admin-only `pgokf_private`).

All SQL below is copy-pasteable. For the exhaustive signature/type reference,
read `docs/sql-api.md`; for config semantics, `docs/configuration.md`; for error
decoding, `docs/troubleshooting.md`.

## 1. Install and grant roles (do this first)

`CREATE EXTENSION` requires a superuser. It installs everything in schema
`pgokf` and creates three cluster-wide `NOLOGIN` roles: `pgokf_reader`,
`pgokf_writer`, and `pgokf_admin`.

```sql
CREATE EXTENSION pgokf;   -- superuser
```

**The 42501-until-granted gotcha.** `USAGE` on schema `pgokf` is revoked from
`PUBLIC`, and every function also runs an in-function membership check against
`session_user`. So a fresh login role can see nothing until a superuser grants
it one of the three roles: until then even `SELECT pgokf.version();` fails with
`42501 permission denied for schema pgokf`.

```sql
-- Grant to real LOGIN roles (the NOLOGIN roles cannot be logged into directly):
GRANT pgokf_reader TO analytics_ro;   -- search / graph / read
GRANT pgokf_writer TO ingest_svc;     -- register / refresh / ingest (the pipeline tier)
GRANT pgokf_admin  TO catalog_ops;    -- config / file-writing exports / purge / index rebuilds
```

The three roles nest: `pgokf_admin` is granted `pgokf_writer`, which is granted
`pgokf_reader`. So a reader may search and read; a writer may additionally
register/refresh/unregister and set embeddings (the intended tier for an
ingestion pipeline); an admin may additionally manage configuration, run the
file-writing exports, purge retired bundles, and rebuild the optional indexes.
Membership is checked against `session_user`, so `SET ROLE` to a role that lacks
membership will still be denied.

Confirm access:

```sql
SELECT pgokf.version();   -- reader-level; returns the loaded crate version
```

## 2. Register, refresh, and manage bundles (writer)

`register_bundle` validates a server-side directory path (absolute, no `..`, no
NUL, must be a canonicalizable directory), then synchronizes every non-reserved
`.md` file into the catalog in one transaction. Parsing is strict: the first
malformed file aborts the whole sync (`22023`) and rolls back - a partial
projection is never committed.

```sql
-- path, name (optional label), options (jsonb stored verbatim for producers)
SELECT * FROM pgokf.register_bundle('/abs/path/to/examples/sample-bundle');
--  bundle_id |  path  | added | updated | removed | unchanged | total
```

```sql
SELECT * FROM pgokf.register_bundle(
    '/srv/okf-bundles/ops',
    'Ops knowledge base',       -- name (DEFAULT NULL)
    '{"team":"platform"}'::jsonb -- options (DEFAULT '{}', stored on bundles.options)
);
```

`refresh_bundle` re-syncs from the stored canonical path, re-parsing only files
whose BLAKE3 content hash changed and removing rows for deleted files:

```sql
SELECT added, updated, removed, unchanged, total FROM pgokf.refresh_bundle(1);
```

Inspect and remove:

```sql
SELECT id, path, name, file_count, enabled FROM pgokf.list_bundles();
SELECT id, file_count, last_synced_at FROM pgokf.bundle_info(1);
SELECT id, path, file_count FROM pgokf.unregister_bundle(1);  -- cascades all rows
```

Registering the same canonical path twice raises `23505` - use `refresh_bundle`
to re-sync instead. Reserved files `index.md` and `log.md` (at any depth) are
skipped and never become concepts.

**Mountless ingestion.** When the bundle does not live on the server's
filesystem, `register_bundle_content(name, paths text[], contents bytea[],
options jsonb DEFAULT '{}')` ingests file contents shipped over the wire in one
call (the `pgokf-ingest` companion CLI drives it, including `--watch`
re-syncing). Same writer tier, same strict parsing.

**Lifecycle (writer, except the purge).** `set_bundle_enabled(id, false)` hides
a bundle from search without deleting it; `retire_bundle(id)` /
`unretire_bundle(id)` mark a bundle retired (excluded from search) while
keeping its rows; the admin-only `purge_retired(older_than interval)` deletes
bundles retired at least that long ago. `unregister_bundle(id)` remains the
immediate cascade delete.

## 3. Search concepts (lexical)

`concept_search(query, bundle_id DEFAULT NULL, limit_count DEFAULT 20,
concept_type DEFAULT NULL, tags DEFAULT NULL, status DEFAULT NULL, trust_tier
DEFAULT NULL, after_cursor DEFAULT NULL)` ranks concepts with PostgreSQL native
FTS by default: `websearch_to_tsquery` matching, `ts_rank_cd` ranking, and a
`ts_headline` snippet per hit. Weights are title `A`, tags/type/description
`B`, body `D`. Only **active** bundles (enabled and not retired) are searched.

```sql
SELECT concept_id, title, type, round(rank::numeric, 4) AS rank, headline
FROM pgokf.concept_search('postgres failover');
```

Query syntax is websearch-style: bare words are AND-ed, `"quoted phrase"` is a
phrase, `or` is alternation, and a leading `-` negates (`postgres -replica`).
`limit_count` must be `1..=500`; the query must be non-empty (`22023`
otherwise).

**Prefer filtering broad queries.** Ranks are comparable only *within one query*
and a broad term ranks every match, so scope the search: by bundle, or with the
built-in structured filters (each a no-op when `NULL`; `tags` is ALL-of
containment; `status`/`trust_tier` come from `concept_provenance`):

```sql
-- Scope to one bundle:
SELECT concept_id, title, rank FROM pgokf.concept_search('incident response', 1, 10);

-- Structured filters, no join needed:
SELECT concept_id, title, rank
FROM pgokf.concept_search('incident response',
                          concept_type => 'Runbook',
                          tags => ARRAY['oncall']);
```

Note the result key is `concept_id` (not `id`) and there is **no** `tags`
column - join `pgokf.concepts` to get tags/description.

**Keyset pagination.** Results have a stable total order (`rank DESC,
bundle_id ASC, concept_id ASC`). To fetch the next page, pass `after_cursor` a
`{rank, bundle_id, concept_id}` jsonb object copied from the previous page's
last row (a malformed cursor raises `22023`):

```sql
SELECT * FROM pgokf.concept_search('incident response', limit_count => 20,
    after_cursor => '{"rank": 0.1024, "bundle_id": 1, "concept_id": "runbooks/x"}'::jsonb);
```

**Companions to a search.** All reader-level:

```sql
-- Faceted counts over the same match set
-- (facet: 'type' | 'bundle' | 'status' | 'trust_tier' | 'tag'):
SELECT facet_value, count FROM pgokf.search_facets('incident response', facet => 'type');

-- More-like-this from a seed concept (lexical, no embeddings needed):
SELECT concept_id, title, rank FROM pgokf.find_similar('runbooks/database-failover', 1, 10);

-- Which backend/indexes are actually in effect (jsonb report):
SELECT jsonb_pretty(pgokf.search_index_status());
```

> Large-scale broad-corpus ranking (BM25) is an **optional, config-selected
> backend**, not a standalone function. Set the durable `search_backend` key to
> `bm25` (via `pgokf.set_config`) to route `concept_search` through a ParadeDB
> `pg_search` index - the operator must install `pg_search` separately, and it
> falls back to native FTS with a warning when absent. There is no `bm25()`
> function; `search_backend` is a config key, and `concept_search` remains the
> only search entry point. See `docs/search-guide.md` (Enabling the BM25
> backend) and `docs/configuration.md` (`search_backend`).

### Semantic and hybrid search (optional, pgvector)

With the `pgvector` extension installed, the catalog also ranks by embedding
similarity. Embeddings are supplied by the caller (the `pgokf-embed` companion
computes and stores them; the extension never calls a model):

```sql
-- Writer: store one embedding per concept (length must equal embedding_dim):
SELECT pgokf.set_concept_embedding(1, 'runbooks/database-failover', ARRAY[0.1, 0.2, ...]::real[]);

-- Reader: cosine-similarity ranking (query_embedding real[], bundle_id, limit_count):
SELECT concept_id, title, rank
FROM pgokf.concept_search_semantic(ARRAY[0.1, 0.2, ...]::real[], NULL, 10);

-- Reader: hybrid lexical + semantic via Reciprocal Rank Fusion (rank = RRF score):
SELECT concept_id, title, rank
FROM pgokf.concept_search_hybrid('database failover', ARRAY[0.1, 0.2, ...]::real[], NULL, 10);

-- Admin: (re)build the HNSW cosine index after bulk-loading embeddings
-- or changing embedding_dim:
SELECT pgokf.rebuild_embedding_index();
```

The vector length is governed by the `embedding_dim` config key. Degradation is
clean: without `pgvector`, `concept_search_semantic` raises `22023` naming the
missing dependency (there is no lexical fallback for a pure vector query),
while `concept_search_hybrid` degrades to lexical-only with a `WARNING`.
`search_index_status()` reports embedding coverage. See
`docs/search-guide.md` for the full semantic/hybrid guide.

## 4. Walk the link graph

Markdown links `[label](target.md)` become directed edges. `concept_neighbors`
walks outward over **resolved, internal** edges only (external `http:`/`mailto:`
and unresolved links are never traversed):

```sql
-- concept_id (path-derived, no .md), max_hops DEFAULT 2, bundle_id DEFAULT NULL
SELECT source_id, neighbor_id, hops, path, title
FROM pgokf.concept_neighbors('runbooks/database-failover', 3, 1)
ORDER BY hops, neighbor_id;
```

`max_hops` must be `>= 1` and is capped at the `pgokf.max_graph_hops` GUC.
If `bundle_id` is `NULL` and the concept ID exists in more than one bundle, the
call raises `22023` asking you to disambiguate. For raw edges/backlinks, query
`pgokf.links` directly:

```sql
SELECT source_id, target_id, link_kind, resolved, is_external
FROM pgokf.links WHERE bundle_id = 1 AND source_id = 'runbooks/database-failover';
-- backlinks:
SELECT source_id FROM pgokf.links
WHERE bundle_id = 1 AND target_id = 'services/postgresql' AND resolved;
```

## 5. Provenance and metadata

Unknown frontmatter keys land losslessly in `pgokf.concept_metadata` (one row
per key, `jsonb` value). The OKF v0.2 provenance / trust / lifecycle families
are additionally projected into three sparse tables - only concepts that carry
such frontmatter get rows:

```sql
-- Scalar generation / lifecycle columns + the derived trust tier:
SELECT concept_id, generated_by, generated_at, status, stale_after, trust_tier
FROM pgokf.concept_provenance ORDER BY concept_id;

-- The ordered verified[] events (one row per event):
SELECT concept_id, ordinal, verified_by, verified_at
FROM pgokf.concept_verification
WHERE bundle_id = 1 ORDER BY concept_id, ordinal;

-- The sources[] provenance materials (one row per entry):
SELECT concept_id, ordinal, source_id, resource, author, usage_count, last_modified
FROM pgokf.concept_provenance_source
WHERE bundle_id = 1 ORDER BY concept_id, ordinal;

-- Arbitrary producer metadata by key:
SELECT concept_id, key, value FROM pgokf.concept_metadata
WHERE bundle_id = 1 AND key = 'quality_band';
```

`generated_by` / `generated_at` come from `generated.{by,at}` (tolerating bare
`generated_by` / `generated_at`); `status` and `stale_after` are the LIFECYCLE
fields (spec default status is `stable`); `usage_window_from` / `usage_window_to`
frame the source usage counts. `trust_tier` is **derived** from the `verified[]`
actors: `human-reviewed` when any is a `human:` actor, else `machine-confirmed`
with ≥1 event, else `unverified`. Every recognized key is also kept verbatim in
the `concept_provenance.details` jsonb. Find, for example, everything a human
reviewed:

```sql
SELECT concept_id, generated_at FROM pgokf.concept_provenance
WHERE bundle_id = 1 AND trust_tier = 'human-reviewed';
```

The bundle's declared OKF version (from the reserved root `index.md`
`okf_version`) is on `pgokf.bundles.okf_version` and in `bundle_info` /
`list_bundles`.

## 6. Raw source retrieval (opt-in tier)

By default the catalog stores no raw source (the enterprise/data-lake tier). To
keep verbatim source bytes in `pgokf.concept_source`, enable the `store_source`
config key **before** registering - it is consulted at sync time:

```sql
SELECT pgokf.set_config('store_source', 'true'::jsonb);   -- admin
SELECT * FROM pgokf.register_bundle('/abs/path/to/bundle');  -- now stores source
```

Then retrieve bytes (reader-level; no filesystem write), or reconstruct the tree
on the server (admin):

```sql
SELECT convert_from(pgokf.get_concept_source(1, 'services/postgresql'), 'UTF8');
SELECT concepts_rows AS files, bytes_written
FROM pgokf.export_sources(1, '/srv/okf-rebuild/sample');   -- admin, writes files
```

With `store_source` off, `get_concept_source` raises `22023`. Toggling the key
does not retroactively populate existing bundles - re-register (or refresh a
changed file) to store their source.

## 7. Configuration and GUCs

Durable, cluster-persistent policy lives in `pgokf_private.config`, managed only
through the config functions (no direct DML). Valid keys and value shapes:

| Key | Shape | Meaning |
| --- | ----- | ------- |
| `allowed_roots` | `["/abs", ...]` | Confine registered/exported paths to these roots |
| `default_text_search_config` | `"pg_catalog.english"` | FTS configuration (must be installed) |
| `default_strict` | `true`/`false` | Strict parsing default |
| `default_exclude` | `["*.tmp", ...]` | Glob patterns skipped during discovery |
| `sync_log_retention_days` | integer `>= 0` | Sync-log retention |
| `store_source` | `true`/`false` | Store verbatim source bytes (section 6) |
| `search_backend` | `"native"`/`"bm25"` | Search strategy for `concept_search` (section 3) |
| `notify_channel` | `"channel"` (`""` disables) | `pg_notify` channel for sync change notifications |
| `okf_version_policy` | `"warn"`/`"reject"` | How a declared-but-unsupported bundle `okf_version` is handled |
| `embedding_dim` | integer | Required embedding vector length (section 3) |
| `track_history` | `true`/`false` | Opt-in concept version history (`concept_history` / `concept_as_of`) |
| `history_retention_days` | integer `>= 0` | History retention |

```sql
SELECT pgokf.set_config('allowed_roots', '["/srv/okf-bundles"]'::jsonb);  -- admin
SELECT jsonb_pretty(pgokf.get_config());                                   -- reader
SELECT pgokf.reset_config('allowed_roots');  -- reset one key to its default
SELECT pgokf.reset_config();                 -- reset every key
```

Unknown keys, wrong-shaped, or out-of-domain values raise `22023`. Changing
`default_text_search_config` is NOT retroactive - existing `body_tsv` vectors
keep the configuration in effect when they were indexed. Set it before the first
`register_bundle`, or re-register a bundle to rebuild its vectors.

Five `pgokf.*` GUCs are hard safety ceilings (`SHOW` to read). The four resource
limits use `SIGHUP` context - set only in `postgresql.conf` + reload, never via
SQL `SET`; `log_level` uses `SUSET` (a superuser can change it at runtime). A
sixth GUC, `pgokf.tenant`, is different in kind: a `USERSET` per-session tenant
selector for the opt-in multi-tenant row-level-security policies (empty, the
default, sees every row). It is a policy selector any session may set, not a
hard security boundary against arbitrary SQL; see `docs/multi-tenancy.md`.

| GUC | Default | Purpose |
| --- | ------- | ------- |
| `pgokf.max_file_bytes` | `4194304` (4 MiB) | Per-file size cap |
| `pgokf.max_bundle_files` | `100000` | Files discovered per bundle |
| `pgokf.max_frontmatter_bytes` | `262144` (256 KiB) | Frontmatter block cap |
| `pgokf.max_graph_hops` | `5` | `concept_neighbors` hop ceiling |
| `pgokf.log_level` | `warning` | Extension log verbosity |

```sql
SHOW pgokf.max_graph_hops;
```

> **`SHOW` needs the library loaded first.** The `pgokf.*` GUCs are registered
> by the extension's shared library, so in a brand-new session `SHOW
> pgokf.max_graph_hops` resolves only **after** the `.so` has loaded - i.e.
> after the session's first `pgokf` call (e.g. `SELECT pgokf.version();`).
> Before that, `SHOW` may report `unrecognized configuration parameter`. Make
> any `pgokf` call first, then `SHOW`.

## 8. Troubleshooting common SQLSTATEs

| SQLSTATE | Meaning | Typical cause / fix |
| -------- | ------- | ------------------- |
| `42501` | Insufficient privilege | Login role not granted `pgokf_reader`/`pgokf_writer`/`pgokf_admin`, or calling a writer/admin function from a lower tier. Grant the role (section 1). |
| `22023` | Invalid parameter | Unknown `bundle_id`; bad path (relative/`..`/NUL/not a dir, or outside `allowed_roots`); empty query; `limit_count` outside `1..=500`; `max_hops < 1`; ambiguous concept ID across bundles; a malformed concept file during sync; a malformed `after_cursor`; an embedding whose length differs from `embedding_dim`; `concept_search_semantic` without `pgvector`; `get_concept_source` with `store_source` off; unknown/wrong-shaped config value. |
| `23505` | Unique violation | Registering an already-registered canonical path. Use `refresh_bundle`. |
| `XX000` | Internal error | e.g. a stored-source integrity mismatch during `export_sources`. |

## 9. The wider surface (pointers)

The full API is 39 functions; beyond the ones shown above, know that these
exist and where they are documented:

- **Observability (reader):** `catalog_stats()`, `health()`,
  `stale_concepts(bundle_id, as_of)`, `duplicate_concepts(bundle_id, limit)`,
  `list_sync_log` / `list_sync_changes`, `list_bundle_log` (the reserved
  `log.md` history). Admin-only: `list_access_log`. See `docs/operations.md`.
- **Version history (opt-in via `track_history`):** `concept_history(bundle_id,
  concept_id, limit)` and point-in-time `concept_as_of(bundle_id, concept_id,
  at)`. See `docs/version-history.md`.
- **Scheduling (admin, needs `pg_cron`):** `schedule_refresh(bundle_id, cron)` /
  `unschedule_refresh(bundle_id)`.
- **Exports (admin):** `export_parquet(bundle_id, dir)` and
  `export_sources(bundle_id, dir)` (section 6); `rebuild_search_index()`
  rebuilds the lexical index.

Optional runtime extensions degrade cleanly when absent: `pgvector`
(semantic/hybrid), `pg_search` (BM25), `pg_cron` (scheduling).

## Full flow (copy-paste)

```sql
-- 1. Superuser: install and grant.
CREATE EXTENSION pgokf;
GRANT pgokf_admin TO catalog_ops;

-- 2. As catalog_ops: confine paths, register a bundle.
SELECT pgokf.set_config('allowed_roots', '["/srv/okf-bundles"]'::jsonb);
SELECT * FROM pgokf.register_bundle('/srv/okf-bundles/ops', 'Ops KB');

-- 3. Search, then explore the graph from the top hit.
SELECT concept_id, title, rank FROM pgokf.concept_search('database failover', 1, 5);
SELECT neighbor_id, hops FROM pgokf.concept_neighbors('runbooks/database-failover', 2, 1)
ORDER BY hops;

-- 4. Read provenance for a concept.
SELECT generated_by, generated_at, status, trust_tier FROM pgokf.concept_provenance
WHERE bundle_id = 1 AND concept_id = 'runbooks/database-failover';

-- 5. Keep it current later.
SELECT added, updated, removed FROM pgokf.refresh_bundle(1);
```

To author concept files that register cleanly, use the `okf-authoring` skill.
