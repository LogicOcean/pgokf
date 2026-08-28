# Getting started with pgokf

This is an end-to-end, copy-pasteable first run. By the end you will have
installed the extension, fixed the one permission gotcha every new operator
hits, confined bundle registration to a directory you control, registered a
bundle of OKF concepts, searched it full-text, walked its link graph, and
retrieved a concept's original source bytes.

Everything below was run against a live PostgreSQL 18 cluster with `pgokf`
`0.1.3` installed; the output blocks are the real output, lightly trimmed for
width. If your numbers differ, it is because your bundle differs — the shapes
will match.

- New to the OKF document format? See [okf-authoring.md](okf-authoring.md) and
  the ready-to-copy [`templates/`](https://github.com/LogicOcean/okf-pg-catalog/blob/main/templates/README.md).
- Choosing where the catalog and the source files should live? See
  [deployment-topologies.md](deployment-topologies.md).
- Running it in production? See [operations.md](operations.md).

---

## Prerequisites

- PostgreSQL 15, 16, 17, or 18 with the `pgokf` extension files installed into
  the cluster's `SHAREDIR/extension` and the shared library into `PKGLIBDIR`
  (see [packaging.md](packaging.md) for how the artifacts get there).
- A superuser connection for the one-time install and role setup.
- A directory of OKF Markdown files to register — this walkthrough uses the
  bundle shipped at [`examples/sample-bundle`](https://github.com/LogicOcean/okf-pg-catalog/blob/main/examples/sample-bundle), which
  contains four concepts (a service, a runbook, a reference appendix, and a
  dashboard) that cross-link each other so the graph and search steps have
  something to show.

Copy the sample bundle somewhere the PostgreSQL server process can read:

```bash
mkdir -p /srv/okf
cp -r examples/sample-bundle /srv/okf/knowledge
```

> **The server reads the path, not your shell.** `register_bundle` is executed
> inside the PostgreSQL backend, so the path must be readable by the OS user
> that runs `postgres` (commonly `postgres`), not by you. A path that works in
> your terminal but lives under `/home/you` will typically fail with a
> permission error from the server side. Put bundles somewhere the server owns —
> `/srv/okf/...` here.

---

## 1. Install the extension (superuser)

Connect as a superuser and create the extension. This runs the bootstrap SQL,
which creates the `pgokf` (public API) and `pgokf_private` (internal state)
schemas, the catalog tables and composite types, and the two cluster-wide roles
`pgokf_reader` and `pgokf_admin`.

```sql
CREATE EXTENSION pgokf;
SELECT pgokf.version();
```

```text
CREATE EXTENSION
 version
---------
 0.1.3
(1 row)
```

`pgokf.version()` reports the loaded shared library's version; after an upgrade,
compare it against the installed SQL version to confirm the two agree (see
[operations.md](operations.md#upgrades)).

> **Make the GUCs cluster-wide.** The `pgokf.*` server settings (resource
> ceilings such as `pgokf.max_file_bytes`) register when the shared library
> loads. The library loads lazily the first time a session calls a `pgokf`
> function, so a brand-new session sees `unrecognized configuration parameter`
> until it touches the extension. To make the ceilings visible in every session
> from connection start — and to be able to set them in `postgresql.conf` — add
> the library to `shared_preload_libraries` and restart:
>
> ```conf
> # postgresql.conf
> shared_preload_libraries = 'pgokf'
> ```
>
> This is optional for a first run but recommended for any real deployment.

---

## 2. The role model, and the 42501 gotcha

`pgokf` ships two `NOLOGIN` roles. You never log in *as* them; you `GRANT` them
to your real login users.

| Role | May do |
| ---- | ------ |
| `pgokf_reader` | Search and read: `concept_search`, `concept_neighbors`, `list_bundles`, `bundle_info`, `get_config`, `get_concept_source`, and `SELECT` on the catalog tables. |
| `pgokf_admin` | Everything a reader can, **plus** `register_bundle`, `refresh_bundle`, `unregister_bundle`, `set_config`, `reset_config`, `export_parquet`, `export_sources`. `pgokf_admin` inherits `pgokf_reader`. |

The bootstrap deliberately **does not** grant either role to `PUBLIC`. Schema
`USAGE` on `pgokf` is granted only to those two roles. So a fresh login role —
even one you just created — gets **SQLSTATE 42501, `permission denied for
schema pgokf`** on its very first query, before it even reaches a function's own
privilege check:

```sql
CREATE ROLE okf_app LOGIN;
SET ROLE okf_app;
SELECT * FROM pgokf.list_bundles();
```

```text
CREATE ROLE
SET
ERROR:  permission denied for schema pgokf
LINE 1: SELECT * FROM pgokf.list_bundles();
                      ^
```

This is not a misconfiguration — it is the least-privilege default working as
designed. The fix is a single grant. Back in your superuser session:

```sql
RESET ROLE;                       -- leave the okf_app role
GRANT pgokf_admin TO okf_app;     -- this user will manage bundles
```

Grant `pgokf_reader` instead for a user that should only search and read. See
[security.md](security.md#roles-and-least-privilege) for the full model,
including why the in-function checks use `session_user`.

---

## 3. Confine registration with `allowed_roots`

`register_bundle` makes the PostgreSQL backend read arbitrary files from the
host filesystem. Before registering anything, set `allowed_roots` so a bundle
path *must* resolve inside a directory you have blessed. Any path outside every
configured root — including one reached through a symlink — is rejected with
SQLSTATE 22023.

`set_config` requires `pgokf_admin`, so do this as `okf_app` (or any admin):

```sql
SET ROLE okf_app;
SELECT pgokf.set_config('allowed_roots', '["/srv/okf"]'::jsonb);
SELECT pgokf.get_config();
```

```text
SET
 set_config
------------

(1 row)

                                   get_config
--------------------------------------------------------------------------------
 {"store_source": false, "allowed_roots": ["/srv/okf"], "default_strict": true,
  "default_exclude": [], "sync_log_retention_days": 30,
  "default_text_search_config": "pg_catalog.english"}
(1 row)
```

`get_config` returns the whole effective policy as one JSON object. The keys and
their meaning are documented in [configuration.md](configuration.md#durable-policy-pgokf_privateconfig);
the two you will care about most on day one are `allowed_roots` (just set) and
`store_source` (covered in step 7).

> **When `allowed_roots` is empty**, the interim policy accepts any absolute,
> traversal-free, canonical path. Setting at least one root is strongly
> recommended for any shared or production cluster — see
> [security.md](security.md#allowed_roots-containment).

---

## 4. Register the bundle

Now register the copied bundle. The second argument is an optional human name.

```sql
SET ROLE okf_app;
SELECT * FROM pgokf.register_bundle('/srv/okf/knowledge', 'knowledge');
```

```text
 bundle_id |        path         | added | updated | removed | unchanged | total
-----------+---------------------+-------+---------+---------+-----------+-------
         1 | /srv/okf/knowledge  |     4 |       0 |       0 |         0 |     4
```

The returned `bundle_sync_result` is the per-bucket file accounting for the
sync: four files discovered, all four newly `added`. Note the returned
`bundle_id` — you pass it to `refresh_bundle`, `bundle_info`, `export_parquet`,
and (optionally) the search and graph functions. The reserved files `index.md`
and `log.md` at every directory level are **not** counted as concepts.

Confirm what is registered:

```sql
SELECT id, name, okf_version, file_count, enabled FROM pgokf.list_bundles();
```

```text
 id |   name    | okf_version | file_count | enabled
----+-----------+-------------+------------+---------
  1 | knowledge |             |          4 | t
```

`okf_version` is blank here because the sample bundle's root `index.md` carries
no `okf_version` frontmatter key. Add `okf_version: "0.2"` to a bundle-root
`index.md` and the catalog will populate this column on the next sync — see
[okf-authoring.md](okf-authoring.md) and the reserved-file rules there.

---

## 5. Full-text search

`concept_search(query, bundle_id DEFAULT NULL, limit_count DEFAULT 20)` returns
ranked hits. The query is a plain phrase; it is parsed with the configured text
search configuration (`pg_catalog.english` by default) and matched against a
weighted `tsvector` built from each concept's title (weight A), tags / type /
description (B), and body (D).

```sql
SELECT bundle_id, concept_id, title, type, round(rank::numeric, 4) AS rank
  FROM pgokf.concept_search('failover');
```

```text
 bundle_id |         concept_id         |       title        |   type    |  rank
-----------+----------------------------+--------------------+-----------+--------
         1 | runbooks/appendix          | Failover appendix  | Reference | 1.6000
         1 | runbooks/database-failover | Database failover  | Runbook   | 1.2000
         1 | services/postgresql        | PostgreSQL service | Reference | 0.3000
```

`concept_id` is the path-derived identity: the bundle-relative path without its
`.md` suffix (`runbooks/database-failover.md` → `runbooks/database-failover`).
`rank` is `ts_rank`, higher is more relevant. The full result also carries a
`headline` snippet with the matching terms highlighted — see
[search-guide.md](search-guide.md) for query syntax, ranking, scoping to a
single bundle, and performance characteristics.

Constrain a search to one bundle by passing its id, and cap the result count
(1–500) with the third argument:

```sql
SELECT concept_id, title FROM pgokf.concept_search('postgresql', 1, 5);
```

---

## 6. Walk the link graph

Concepts link to each other (in the sample bundle the service points at its
runbook and dashboard, the runbook at its appendix). `concept_neighbors(concept_id,
max_hops DEFAULT 2, bundle_id DEFAULT NULL)` returns every concept reachable
within `max_hops`, with the shortest hop count and the path taken:

```sql
SELECT source_id, neighbor_id, hops, title
  FROM pgokf.concept_neighbors('services/postgresql', 2);
```

```text
      source_id      |        neighbor_id         | hops |       title
---------------------+----------------------------+------+-------------------
 services/postgresql | dashboards/health          |    1 | Service health
 services/postgresql | runbooks/database-failover |    1 | Database failover
 services/postgresql | runbooks/appendix          |    2 | Failover appendix
```

`max_hops` is capped by the `pgokf.max_graph_hops` GUC (default 5), so a caller
cannot ask for an unbounded traversal. The full `concept_neighbor` row also
returns `path`, the array of concept ids from the start to that neighbor.

---

## 7. Retrieve a concept's source

Whether the original file bytes live *inside* PostgreSQL is governed by the
`store_source` policy key, and it is the single biggest deployment decision —
see [deployment-topologies.md](deployment-topologies.md).

`store_source` defaults to **false** (the enterprise/data-lake tier: PostgreSQL
holds metadata and the search index, the files stay in their external store).
With it off, `get_concept_source` tells you plainly that no bytes are stored:

```sql
SELECT octet_length(pgokf.get_concept_source(1, 'runbooks/database-failover'));
```

```text
ERROR:  no source is stored for concept runbooks/database-failover in bundle 1;
        the bundle was synced with store_source disabled
```

To make PostgreSQL self-contained (the small tier), enable `store_source` and
re-index. **`store_source` is not retroactive**, and `refresh_bundle`
re-projects only files whose content changed — so on an unchanged bundle a plain
refresh will not backfill the sources:

```sql
SELECT pgokf.set_config('store_source', 'true'::jsonb);
SELECT * FROM pgokf.refresh_bundle(1);
```

```text
 bundle_id |        path         | added | updated | removed | unchanged | total
-----------+---------------------+-------+---------+---------+-----------+-------
         1 | /srv/okf/knowledge  |     0 |       0 |       0 |         4 |     4
```

All four `unchanged` — nothing was re-projected, so still no stored bytes. To
force a full re-projection of an unchanged bundle, unregister and register it
again (or edit the files). Re-registering assigns a fresh `bundle_id`:

```sql
SELECT id FROM pgokf.unregister_bundle(1);
SELECT bundle_id FROM pgokf.register_bundle('/srv/okf/knowledge', 'knowledge');
-- bundle_id is now 2
SELECT octet_length(pgokf.get_concept_source(2, 'runbooks/database-failover'))
       AS source_bytes;
```

```text
 source_bytes
--------------
         1096
```

The stored bytes are the exact, unmodified source file (they hash to
`pgokf.concepts.file_hash`). Decode them as text with `convert_from(..., 'UTF8')`.
The retrieval nuance above is exactly the kind of thing
[operations.md](operations.md#backfilling-stored-sources) covers for day-2 work.

---

## 8. (Optional) Export for analytics or DR

An admin can snapshot a bundle's catalog projection to Parquet, and — when
sources are stored — export the original files back to disk:

```sql
-- concepts / metadata / links / provenance as four Parquet files
SELECT bundle_id, concepts_rows, metadata_rows, links_rows,
       provenance_rows, bytes_written
  FROM pgokf.export_parquet(2, '/srv/okf/export');

-- the stored source files, reconstructed under dest_dir (store_source only)
SELECT bundle_id, concepts_rows, bytes_written
  FROM pgokf.export_sources(2, '/srv/okf/export');
```

```text
 bundle_id | concepts_rows | metadata_rows | links_rows | provenance_rows | bytes_written
-----------+---------------+---------------+------------+-----------------+---------------
         2 |             4 |             9 |         12 |               4 |         15272

 bundle_id | concepts_rows | bytes_written
-----------+---------------+---------------
         2 |             4 |          2615
```

`dest_dir` must already exist, be writable by the server, and — when
`allowed_roots` is set — resolve inside one of the roots, exactly like a bundle
path. The Parquet files are interoperable with tools such as DuckDB. See
[operations.md](operations.md#export-for-analytics-and-dr) for using exports in
backup/DR and analytics pipelines.

---

## Where to go next

| You want to… | Read |
| ------------ | ---- |
| Write your own OKF concepts | [okf-authoring.md](okf-authoring.md), [`templates/`](https://github.com/LogicOcean/okf-pg-catalog/blob/main/templates/README.md) |
| Decide where catalog and files live | [deployment-topologies.md](deployment-topologies.md) |
| Run it day-to-day | [operations.md](operations.md) |
| Get better search results | [search-guide.md](search-guide.md) |
| Every function, table, and type | [sql-api.md](sql-api.md) |
| Tune ceilings and policy | [configuration.md](configuration.md) |
| Understand the security model | [security.md](security.md) |
| Diagnose an error | [troubleshooting.md](troubleshooting.md) |

## Tear down (for a scratch run)

```sql
DROP EXTENSION pgokf;   -- drops the pgokf / pgokf_private schemas and their objects
```

The two roles are cluster-wide and survive `DROP EXTENSION` (they may be shared
across databases); drop them explicitly with `DROP ROLE pgokf_reader,
pgokf_admin;` only if nothing else uses them.
