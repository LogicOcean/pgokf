# Operations

Day-2 guidance for running `pgokf`: what to monitor, how to schedule refreshes,
how to back up and restore, how to upgrade across versions, how to tune the
ceilings and policy, how to reason about capacity, and how to use the export
functions for analytics and disaster recovery.

Everything here is expressed through the real SQL surface. For the reference
descriptions of each function and table see [sql-api.md](sql-api.md); for the
knobs see [configuration.md](configuration.md); for the trust boundary see
[security.md](security.md); for first-run setup see
[getting-started.md](getting-started.md).

---

## Monitoring

The catalog is ordinary PostgreSQL tables and functions, so your existing
PostgreSQL monitoring applies. On top of that, watch these `pgokf`-specific
signals.

### Catalog health

`list_bundles()` is the operator's dashboard query. `last_synced_at` going stale
is the first sign a scheduled refresh has stopped running.

```sql
SELECT id, name, okf_version, file_count, enabled, last_synced_at
  FROM pgokf.list_bundles()
 ORDER BY last_synced_at NULLS FIRST;
```

Cross-check the projected corpus against what each bundle claims:

```sql
-- concepts actually indexed, per bundle
SELECT bundle_id, count(*) AS concepts
  FROM pgokf.concepts
 GROUP BY bundle_id
 ORDER BY bundle_id;

-- a mismatch between file_count and indexed concepts is worth investigating
SELECT b.id, b.name, b.file_count,
       (SELECT count(*) FROM pgokf.concepts c WHERE c.bundle_id = b.id) AS indexed
  FROM pgokf.bundles b;
```

### Search-index residency

Broad ranked searches are only fast while the `body_tsv` GIN index is
RAM-resident (see [Capacity](#capacity-and-scaling)). Watch its cache hit ratio;
a falling ratio predicts rising broad-query latency.

```sql
SELECT indexrelname,
       idx_blks_hit,
       idx_blks_read,
       round(100.0 * idx_blks_hit
             / nullif(idx_blks_hit + idx_blks_read, 0), 2) AS hit_pct
  FROM pg_statio_user_indexes
 WHERE schemaname = 'pgokf'
 ORDER BY idx_blks_read DESC;
```

Size the catalog objects so you know what needs to fit in `shared_buffers` and
the OS page cache:

```sql
SELECT relname,
       pg_size_pretty(pg_total_relation_size('pgokf.' || relname)) AS total
  FROM (VALUES ('concepts'), ('concept_metadata'), ('links'),
               ('concept_source'), ('bundles')) AS t(relname)
 ORDER BY pg_total_relation_size('pgokf.' || relname) DESC;
```

### What to alert on

- `last_synced_at` older than your refresh interval + margin → refresh pipeline
  broken.
- GIN index `hit_pct` trending down on a growing corpus → broad-search latency
  will climb; add RAM, pre-filter, or adopt the future BM25 adapter.
- Sync errors in the PostgreSQL log (SQLSTATE `22023` invalid parameter, `23505`
  duplicate path, parse failures) — see [troubleshooting.md](troubleshooting.md).
- For the enterprise tier: the lake mount unavailable → registers/refreshes for
  bundles under it will fail (see
  [deployment-topologies.md](deployment-topologies.md)).

---

## Refresh scheduling

`register_bundle` ingests a bundle once. `refresh_bundle(bundle_id)` re-syncs it,
and is **incremental**: it hashes each file (BLAKE3), compares against the stored
`file_hash`, and re-projects only what changed. Its `bundle_sync_result` reports
exactly what moved.

```sql
SELECT * FROM pgokf.refresh_bundle(1);
```

```text
 bundle_id | path | added | updated | removed | unchanged | total
-----------+------+-------+---------+---------+-----------+-------
         1 | ...  |     0 |       2 |       1 |        41 |    44
```

Because it is incremental and idempotent, `refresh_bundle` is safe to run on a
schedule. Drive it from `cron`, a systemd timer, `pg_cron`, or your orchestrator.
A minimal cron shape (an admin login role, membership in `pgokf_admin`):

```bash
# refresh every bundle every 15 minutes
*/15 * * * *  psql "$OKF_DSN" -Atc \
  "SELECT id FROM pgokf.list_bundles() WHERE enabled" \
  | while read -r id; do \
      psql "$OKF_DSN" -c "SELECT * FROM pgokf.refresh_bundle($id);"; \
    done
```

Guidance:

- **Match the interval to change velocity and store latency.** A local
  small-tier bundle can refresh often. An object-store mount pays list + read
  latency on every refresh (it re-walks the mount), so refresh those less
  aggressively — see
  [deployment-topologies.md](deployment-topologies.md).
- **Refreshes serialize per bundle.** Each `refresh_bundle` takes an advisory
  lock keyed on the bundle's canonical path, so overlapping runs of the same
  bundle wait rather than corrupt each other. Different bundles refresh
  concurrently.
- **`strict` decides what a bad file does.** With `default_strict = true` (the
  default) a malformed file aborts the whole sync; set it false to log-and-skip.
  See [configuration.md](configuration.md#which-keys-the-current-engine-consults).
- **Run refreshes on the primary.** They are `VOLATILE` writes; standbys serve
  reads only (see
  [deployment-topologies.md](deployment-topologies.md#scaling-reads-with-replicas)).

> **`sync_log_retention_days` is currently a no-op.** The key exists in
> `pgokf_private.config` and is accepted, but the current engine does not act on
> it — there is no sync-log table to prune yet. Do not rely on it for retention;
> it is reserved for a future sync-history feature.

---

## Backup and restore

`pgokf` state is entirely inside PostgreSQL, so **`pg_dump` / `pg_restore` and
PITR are the backup story.** What a dump captures depends on the tier:

- **Small tier (`store_source = true`)** — the dump includes
  `pgokf.concept_source`, so the **original files travel with the backup**. A
  restore reconstructs a byte-identical corpus with no external dependency.
- **Enterprise tier (`store_source = false`)** — the dump includes metadata and
  the search index but **not** the source bytes (they live in the lake). Back up
  the object store on its own schedule; the two backups together are your DR set.

```bash
# whole database (roles are cluster-wide — see the note below)
pg_dump --format=custom --file=okf.dump "$OKF_DSN"

# just the catalog schemas
pg_dump --format=custom --file=okf-schemas.dump \
        --schema=pgokf --schema=pgokf_private "$OKF_DSN"
```

```bash
pg_restore --dbname="$OKF_DSN" okf.dump
```

Notes:

- **Roles are cluster-wide** and are **not** captured by a database-scoped
  `pg_dump`. Back them up with `pg_dumpall --roles-only`, or recreate them by
  installing the extension (the bootstrap creates `pgokf_reader` / `pgokf_admin`
  idempotently) before restoring, then re-`GRANT` them to your login users.
- **`pgokf_private.config` is in the dump**, so `allowed_roots`, `store_source`,
  and the rest of your policy restore with the catalog. Re-verify `allowed_roots`
  points at valid paths on the restore host.
- **GUC ceilings are not catalog data.** `pgokf.max_file_bytes` and friends live
  in `postgresql.conf`; carry them with your config management, not the dump.
- For point-in-time recovery and streaming standbys, `pgokf` needs nothing
  special — it is table and function data and rides WAL like everything else.

---

## Upgrades

Two versions must agree: the installed **SQL** version and the loaded **shared
library**. Upgrade the SQL objects with `ALTER EXTENSION`:

```sql
-- to the newest installed version
ALTER EXTENSION pgokf UPDATE;

-- or step to a specific version
ALTER EXTENSION pgokf UPDATE TO '0.1.3';

SELECT extversion FROM pg_extension WHERE extname = 'pgokf';
SELECT pgokf.version();   -- the loaded library's version
```

`pgokf` ships an explicit migration chain, so PostgreSQL can walk intermediate
steps for you: the packaged scripts are `0.1.0 → 0.1.1 → 0.1.2 → 0.1.3` (plus the
base `0.1.0` / `0.1.2` / `0.1.3` full installs). `ALTER EXTENSION pgokf UPDATE`
applies the necessary steps in order.

Procedure:

1. Install the new artifacts into the cluster (new `.sql` migration scripts into
   `SHAREDIR/extension`, new shared library into `PKGLIBDIR`) — see
   [packaging.md](packaging.md).
2. If the library was already loaded by running backends, **reconnect** (or
   restart if it is in `shared_preload_libraries`) so the new `.so` is in memory.
3. Run `ALTER EXTENSION pgokf UPDATE`.
4. Confirm `pg_extension.extversion` and `pgokf.version()` **match**. A mismatch
   means the SQL was updated but the old library is still loaded (reconnect), or
   the library was replaced but `ALTER EXTENSION UPDATE` was not run.

See [api-stability.md](api-stability.md) for what may change across versions and
[release-checklist.md](release-checklist.md) for the release process itself.

---

## Tuning: ceilings and policy

Two surfaces, deliberately split (full detail in
[configuration.md](configuration.md)):

**GUC ceilings** — hard, per-cluster safety limits set only in `postgresql.conf`
(they use the `SIGHUP` context and cannot be raised from a SQL session):

```conf
# postgresql.conf
pgokf.max_file_bytes = 8388608        # 8 MiB; raise for large concept files
pgokf.max_bundle_files = 250000       # raise for very large bundles
pgokf.max_frontmatter_bytes = 262144  # 256 KiB YAML frontmatter cap
pgokf.max_graph_hops = 8              # ceiling for concept_neighbors(max_hops)
pgokf.log_level = warning
```

```sql
SELECT pg_reload_conf();      -- apply the SIGHUP changes
SHOW pgokf.max_file_bytes;    -- verify (library must be loaded in the session)
```

Tuning intent:

- **`max_file_bytes`** — a sync aborts on any file larger than this. Raise it if
  legitimate concepts exceed 4 MiB; keep it as low as your real files allow so an
  accidental huge file can't blow up a sync.
- **`max_bundle_files`** — bounds discovery per bundle; a guard against pointing
  `register_bundle` at an enormous tree by mistake.
- **`max_frontmatter_bytes`** — bounds YAML parsing per document.
- **`max_graph_hops`** — the hard ceiling `concept_neighbors(max_hops)` is capped
  to; the guardrail against an unbounded traversal.

**Durable policy** — catalog behavior managed through SQL, persisted in
`pgokf_private.config`, edited only via the admin functions:

```sql
SELECT pgokf.set_config('default_text_search_config',
                        '"pg_catalog.english"'::jsonb);   -- indexing language
SELECT pgokf.set_config('default_strict', 'false'::jsonb); -- skip bad files
SELECT pgokf.set_config('default_exclude', '["drafts/**"]'::jsonb);
SELECT pgokf.get_config();                                 -- read effective policy
```

Policy changes that affect indexing (`default_text_search_config`,
`store_source`, `default_exclude`, `default_strict`) are **not retroactive**:
they take effect for bundles synced or refreshed afterward, and since refresh
re-projects only changed files, a full re-index of an unchanged bundle means
re-registering it (next section).

---

## Backfilling stored sources

Turning on `store_source` (or changing the indexing language) does **not**
rewrite existing rows, and `refresh_bundle` re-projects only files whose content
changed — so an unchanged bundle is left as-is:

```sql
SELECT pgokf.set_config('store_source', 'true'::jsonb);
SELECT * FROM pgokf.refresh_bundle(1);
--  added | updated | removed | unchanged | total
--      0 |       0 |       0 |         4 |     4    ← nothing re-projected
```

To force a full re-projection (backfill the source bytes, or re-tokenize under a
new text-search config), re-register the bundle. This assigns a fresh
`bundle_id`:

```sql
SELECT id FROM pgokf.unregister_bundle(1);              -- cascades away the old rows
SELECT bundle_id FROM pgokf.register_bundle('/srv/okf/knowledge', 'knowledge');
```

Then `get_concept_source` / `export_sources` return the stored bytes. Anything
referencing the old `bundle_id` (dashboards, saved queries) must be repointed at
the new one — plan the swap accordingly.

---

## Capacity and scaling

These are **measured** characteristics of this project's search path. Use them to
size hardware and set expectations; do not extrapolate other numbers from them.

| Query shape | Behavior |
| ----------- | -------- |
| Selective / point / tag / type recall | Sub-millisecond to roughly ~10 ms, holding up to ~10M concepts (index-backed). |
| Broad "rank everything" FTS | Scales **linearly** with corpus size: ≈322 ms @ 1M → ≈2.4 s @ 10M → ≈29 s @ 50M. |
| Broad query with a benchmarked BM25 adapter | Top-k stays flat at ≈10–15 ms even on broad queries (30–194× faster on the broad case). |

Reading these:

- **Selective queries scale well.** Point lookups, tag filters (`concepts_tags_gin`),
  type filters (`concepts_type_idx`), and path lookups (`concepts_path_idx`) stay
  fast into the millions of rows. Design search UIs to **pre-filter** — by
  bundle, tag, or type — before ranking, and you stay in this regime. Passing a
  `bundle_id` to `concept_search` scopes it to one bundle.
- **Broad ranked search grows linearly.** A "rank the whole corpus against this
  phrase" query is the expensive case, reaching seconds at tens of millions of
  concepts. Two levers: **pre-filter** so fewer rows are ranked, and keep the
  `body_tsv` GIN index resident in RAM (below).
- **BM25 is a benchmarked *future* adapter, not a shipped function.** The flat
  top-k numbers above come from evaluating an external BM25 backend
  (ParadeDB `pg_search`); it is not part of `pgokf` today. See
  [bm25-research.md](bm25-research.md). Do not document or call a BM25 function —
  none exists in this release.

For horizontal read scaling, add streaming replicas and route searches to them —
see [deployment-topologies.md](deployment-topologies.md#scaling-reads-with-replicas).

### Keep the GIN index RAM-resident

Broad-search latency is dominated by whether `concepts_body_tsv_gin` is served
from memory. Size `shared_buffers` and leave enough OS page cache so the index
(and the hot parts of `pgokf.concepts`) stay resident; use the residency query in
[Monitoring](#search-index-residency) to confirm the hit ratio stays high as the
corpus grows. When it starts falling and broad queries slow down, the options
are: add RAM, pre-filter harder, or adopt the future BM25 adapter.

---

## Export for analytics and DR

Two admin functions turn a bundle into portable files on the server (both
require `pgokf_admin`; `dest_dir` must exist, be server-writable, and — when
`allowed_roots` is set — resolve inside a root, exactly like a bundle path):

```sql
-- catalog projection → four Parquet files (concepts / metadata / links / provenance)
SELECT * FROM pgokf.export_parquet(1, '/srv/okf/export');

-- stored source files → reconstructed under dest_dir (small tier only)
SELECT * FROM pgokf.export_sources(1, '/srv/okf/export');
```

`export_parquet` returns per-file row counts and total bytes written:

```text
 bundle_id | concepts_rows | metadata_rows | links_rows | provenance_rows | bytes_written
-----------+---------------+---------------+------------+-----------------+---------------
         1 |             4 |             9 |         12 |               4 |         15272
```

Uses:

- **Analytics.** The Parquet files are interoperable with columnar tools — this
  project verified them readable in **DuckDB** — so you can query the catalog
  offline, join it against other datasets, or feed a lakehouse without touching
  the live database.
- **Disaster recovery / portability.** `export_parquet` plus `export_sources`
  (small tier) is a self-describing, engine-independent snapshot of a bundle:
  metadata *and* originals as open files, restorable or auditable without a
  running `pgokf`. It complements `pg_dump` rather than replacing it — `pg_dump`
  is the operational backup; the Parquet/source export is the portable,
  long-lived, tool-agnostic copy.
- **`export_sources` needs stored sources.** It only works in the small tier
  (`store_source = true`); in the enterprise tier the originals already live in
  the lake, which is where you back them up.

---

## Runbook: routine day-2 tasks

| Task | Do this |
| ---- | ------- |
| Re-sync content | `SELECT * FROM pgokf.refresh_bundle(<id>);` (incremental; safe on a schedule) |
| See what's registered | `SELECT * FROM pgokf.list_bundles();` |
| Change indexing language / policy | `pgokf.set_config(...)`, then re-register affected bundles to re-index |
| Turn on stored sources | `set_config('store_source','true')`, then **re-register** each bundle |
| Remove a bundle | `SELECT pgokf.unregister_bundle(<id>);` (cascades all its rows) |
| Back up (small tier) | `pg_dump` — captures metadata, index, and sources |
| Back up (enterprise tier) | `pg_dump` for metadata/index **+** back up the lake separately |
| Upgrade | install artifacts → reconnect → `ALTER EXTENSION pgokf UPDATE` → check versions match |
| Raise a ceiling | edit `postgresql.conf` → `SELECT pg_reload_conf();` |
| Snapshot for analytics/DR | `pgokf.export_parquet(<id>, dir)` (+ `export_sources` on the small tier) |

See also: [getting-started.md](getting-started.md) ·
[deployment-topologies.md](deployment-topologies.md) ·
[configuration.md](configuration.md) · [security.md](security.md) ·
[sql-api.md](sql-api.md) · [troubleshooting.md](troubleshooting.md) ·
[benchmarks.md](benchmarks.md).
