# Docker Compose deployment

The reference production deployment of a `pgokf` catalog on one Docker host:
the server image with every optional extension, an embedding daemon, verified
backups, and optional object-store ingestion and MCP services. It ships in the
repository as [`deploy/compose/`](https://github.com/LogicOcean/pgokf/tree/main/deploy/compose)
and is what CI validates on every packaging change.

It runs unchanged on x86 and arm64 hosts (Linux servers and Apple Silicon
alike): both images are published as multi-architecture manifests. CI renders
the stack on every packaging change and smoke-tests the images on both
architectures; the full stack (embedding daemon, BM25 as the reader role,
backup and restore) was exercised end to end on an arm64 Docker host against a
local Ollama embedding server before release.

For the storage-tier decision behind it (files in PostgreSQL vs. in a lake)
see [deployment-topologies.md](deployment-topologies.md); for day-2
operations see [operations.md](operations.md); for the knobs see
[configuration.md](configuration.md).

---

## What the stack contains

| Service | Image | Role |
| ------- | ----- | ---- |
| `db` | `ghcr.io/logicocean/pgokf:<version>-pg18` | PostgreSQL 18 with `pgokf`, **pgvector** (semantic / hybrid search), **pg_cron** (in-database scheduled refresh), and **Tiger Data pg_textsearch** (BM25 ranking, PostgreSQL license) installed and created on first init (pgokf, pg_cron, and pg_textsearch preloaded); GUC ceilings and memory sizing set from `.env`; OKF bundles bind-mounted read-only at `/bundles`; least-privilege login roles and the catalog policy applied on first initialization. |
| `embed` | `ghcr.io/logicocean/pgokf-companions:<version>` | `pgokf-embed --watch`: every `OKF_EMBED_INTERVAL` seconds, embeds concepts that have no vector yet against your OpenAI-compatible embeddings server. |
| `backup` (profile `ops`) | server image | One-shot `pgokf-backup`: verified `pg_dump` archive + roles dump + checksums, with retention. Driven from cron. |
| `ingest` (profile `ingest`) | companions image | `pgokf-ingest --watch`: mountless ingestion of a bucket-hosted bundle. |
| `mcp` (profile `tools`) | companions image | `pgokf-mcp` over stdio for AI-agent clients, as the reader role. |

The network is **external** (created once, never owned by the stack) so
`docker compose down` cannot destroy it out from under other containers that
attach to it.

---

## Prerequisites

- Docker Engine with Compose v2 (`docker compose`), on x86_64 or arm64.
- A fast local disk for the cluster data, a directory tree for the OKF
  bundles, and a directory for backups.
- The images. On a version tag CI publishes both to GHCR for `linux/amd64`
  and `linux/arm64`; between releases, or for a private build, build them
  locally from the repository root (the build is native to the daemon's
  architecture; see [packaging/docker/README.md](https://github.com/LogicOcean/pgokf/blob/main/packaging/docker/README.md)):

  ```bash
  docker build -f packaging/docker/Dockerfile --build-arg PG_MAJOR=18 -t pgokf:0.1.16-pg18 .
  docker build -f packaging/docker/Dockerfile.companions -t pgokf-companions:0.1.16 .
  ```

- An OpenAI-compatible embeddings endpoint if you want semantic / hybrid
  search: Ollama, vLLM, `text-embeddings-inference`, or OpenAI itself. Note
  the **dimension** its model returns; it goes into the policy below.

---

## Host layout

```bash
sudo install -d -o "$USER" /srv/pgokf/data /srv/pgokf/bundles /srv/pgokf/backups
docker network create pgokf-net
mkdir -p ~/services/pgokf && cp deploy/compose/{docker-compose.yml,.env.example} ~/services/pgokf/
cd ~/services/pgokf && cp .env.example .env && chmod 600 .env
```

Any paths work; the three directories are wired in through `.env`. The
cluster data directory is bind-mounted at the image's `PGDATA`
(`/var/lib/postgresql/18/docker` for the PostgreSQL 18 image; set
`PGOKF_PGDATA` for another major). The entrypoint takes ownership of it on
first start.

---

## Configuration (`.env`)

Every knob is in [`deploy/compose/.env.example`](https://github.com/LogicOcean/pgokf/blob/main/deploy/compose/.env.example);
the ones that matter most:

| Key | Meaning |
| --- | ------- |
| `PGOKF_IMAGE`, `PGOKF_COMPANIONS_IMAGE` | The two image references (GHCR tag or your local tag). |
| `PGOKF_DATA_DIR`, `PGOKF_BUNDLES_DIR`, `PGOKF_BACKUP_DIR` | The three host directories. |
| `PGOKF_PRELOAD` | `shared_preload_libraries` for the server; default `pgokf,pg_cron,pg_textsearch` fits the PostgreSQL 17/18 images. Set `pgokf,pg_cron` for a 15/16/19 image (no BM25 provider ships there) or `pgokf,pg_cron,pg_search` for an image built with ParadeDB; a preloaded library the image lacks stops the server at startup. |
| `PGOKF_BIND_ADDR`, `PGOKF_PORT` | Interface and port to publish PostgreSQL on. **Loopback by default**; use a private or VPN address to reach it from other hosts. Never a public interface without TLS and a firewall (see [Exposure](#exposure-and-tls)). |
| `POSTGRES_PASSWORD`, `PGOKF_ADMIN_PASSWORD`, `PGOKF_WRITER_PASSWORD`, `PGOKF_READER_PASSWORD` | The superuser and the three tier accounts. Generate with `openssl rand -hex 24`: hex is URL-safe (the companions embed these in connection URLs) and contains no `$` (which compose interpolates; a literal one is `$$`). The `PGOKF_*` passwords may also be supplied as files through `PGOKF_*_PASSWORD_FILE` (compose secrets). |
| `OKF_EMBED_TENANT`, `OKF_INGEST_TENANT`, `OKF_MCP_TENANT` | Optional `pgokf.tenant` scope for each companion's session (see [multi-tenancy](multi-tenancy.md#requiring-a-tenant-require_tenant)); required once the catalog policy `require_tenant` is on. |
| `PGOKF_POLICY` | JSON applied through `pgokf.set_config` on first init. **`embedding_dim` must equal your model's output dimension** (1024 for `Qwen3-Embedding-0.6B`, 768 for `nomic-embed-text`, 1536 for `text-embedding-3-small`); `store_source: true` keeps the source bytes in PostgreSQL so one dump is a complete backup; `allowed_roots: ["/bundles"]` confines registration to the mount; `search_backend` is `native` or `bm25`. |
| `OKF_EMBED_ENDPOINT`, `OKF_EMBED_MODEL`, `OKF_EMBED_API_KEY` | Base URL (without `/v1/embeddings`), model name, optional bearer token. |
| `PGOKF_SHARED_BUFFERS`, `PGOKF_EFFECTIVE_CACHE_SIZE`, `PGOKF_MAINTENANCE_WORK_MEM`, `PGOKF_WORK_MEM`, `PGOKF_SHM_SIZE` | Memory sizing. A common starting point is 25 % of RAM for `shared_buffers` and 50-75 % for `effective_cache_size`; the BM25 index and ANN index builds like a generous `maintenance_work_mem`. |
| `PGOKF_MAX_FILE_BYTES`, `PGOKF_MAX_BUNDLE_FILES`, `PGOKF_MAX_FRONTMATTER_BYTES`, `PGOKF_MAX_GRAPH_HOPS` | The hard GUC ceilings ([configuration.md](configuration.md#gucs-resource-ceilings)). |

The roles and policy are applied **once**, when the data directory is
empty. Change them later with SQL (`ALTER ROLE ... PASSWORD`,
`pgokf.set_config`) - editing `.env` after the first start does not re-run
the hooks. If a hook fails (an unknown policy key, a role name that already
exists), the image refuses every later start of that data directory with an
explanation rather than coming up half-provisioned: fix `.env`, empty the
data directory, start again.

---

## First start and verification

```bash
docker compose up -d
docker compose ps                       # db healthy, embed running
docker compose logs db | grep "pgokf initdb"
```

```bash
docker compose exec db psql -U postgres -d okf -c "SELECT extname, extversion FROM pg_extension ORDER BY 1;"
docker compose exec db psql -U postgres -d okf -c "SELECT jsonb_pretty(pgokf.health());"
docker compose exec db psql -U postgres -d okf -c "SELECT jsonb_pretty(pgokf.get_config());"
```

`pg_extension` should list `pgokf`, `vector`, `pg_cron`, and `pg_textsearch`;
`health()` should report `"ok": true` with `"bundle_count": 0`; `get_config()`
should echo your policy.

---

## Loading bundles

Put each OKF bundle under the bundles directory - for example
`/srv/pgokf/bundles/handbook/` - and register it from inside the container
path, as the writer role. `allowed_roots` guarantees nothing outside
`/bundles` can be registered.

```bash
docker compose exec db psql -U okf_writer -d okf \
  -c "SELECT * FROM pgokf.register_bundle('/bundles/handbook', 'handbook');"
```

Keep it fresh either from outside (`refresh_bundle` on a schedule, see
[operations.md](operations.md#refresh-scheduling)) or in-database with the
bundled pg_cron, which needs no external scheduler at all:

```sql
-- as okf_admin: refresh bundle 1 every 15 minutes
SELECT pgokf.schedule_refresh(1, '*/15 * * * *');
```

The mountless alternative - a bundle that lives in an S3-compatible bucket -
is the `ingest` profile: fill in the `OKF_S3_*` / `AWS_*` / `OKF_BUNDLE_NAME`
keys and start it with `docker compose --profile ingest up -d`. It re-lists
the bucket every `OKF_INGEST_INTERVAL` seconds and resyncs on change.

---

## Embeddings and semantic search

The `embed` service is a daemon: on each pass it asks the catalog for concepts
without a vector, embeds them in batches, and stores them through
`pgokf.set_concept_embedding`. New or refreshed content is therefore embedded
within one interval with no operator action. Watch it:

```bash
docker compose logs -f embed
docker compose exec db psql -U okf_reader -d okf -c "SELECT jsonb_pretty(pgokf.search_index_status());"
```

`embedding.coverage_pct` climbs to 100 as the backlog drains. After the first
bulk load build the ANN index once (and again after any large ingest):

```sql
-- as okf_admin
SELECT pgokf.rebuild_embedding_index();
```

Then query with a vector computed by the **same endpoint and model**:

```sql
SELECT concept_id, title, rank FROM pgokf.concept_search_semantic($1::real[]);
SELECT concept_id, title, rank FROM pgokf.concept_search_hybrid('failover', $1::real[]);
```

If the model changes, reset `embedding_dim` if needed, delete the stored
vectors, and let the daemon re-embed - see
[search-guide.md](search-guide.md#semantic-and-hybrid-search-optional-pgvector).

---

## BM25 ranking with pg_textsearch

The image ships Tiger Data `pg_textsearch` (PostgreSQL license) preloaded and
created, so switching the broad-query ranking to BM25 top-k is two statements
as `okf_admin`:

```sql
SELECT pgokf.set_config('search_backend', '"bm25"'::jsonb);
SELECT pgokf.rebuild_search_index();
```

`concept_search` keeps its signature and result shape; only the strategy
changes. The `bm25_provider` policy key stays at `auto`, which resolves to
`pg_textsearch` on this image (`search_index_status()` shows the resolved
provider). Until the index exists searches fall back to native FTS with a
warning. See [search-guide.md](search-guide.md#enabling-the-bm25-backend)
for when BM25 wins and when native does, and for the provider comparison
(an image built with `--build-arg WITH_PG_SEARCH=1 --build-arg WITH_PG_TEXTSEARCH=0`
carries ParadeDB `pg_search` instead; preload `pg_search` in that case).

---

## Backups and restore

Backups are logical and run by the image's own tooling against the live
server, so they need no host PostgreSQL install:

```bash
docker compose run --rm backup
ls /srv/pgokf/backups
#   okf-20260904T031500Z.dump   roles-20260904T031500Z.sql   okf-20260904T031500Z.sha256
```

Schedule it from the deploying user's crontab (no root required to invoke it;
the container writes the files as root, so they land root-owned in the
backups directory):

```cron
0 3 * * *  cd /home/you/services/pgokf && docker compose run --rm backup >> backup.log 2>&1
```

Each run verifies the archive with `pg_restore --list` before publishing it
and prunes artifacts older than `PGOKF_BACKUP_RETENTION_DAYS`. With
`store_source: true` the archive contains the metadata, the search index
tables, **and the original source bytes**; the bundles directory is still
worth backing up on its own for the enterprise tier
([deployment-topologies.md](deployment-topologies.md#the-one-decision-store_source)).

**Restore** into an empty stack (same or newer pgokf version in the image;
the data directory empty so the init hooks recreate the extensions, the login
roles, and a baseline policy):

```bash
docker compose up -d db
docker compose run --rm backup pgokf-restore /backups/okf-<stamp>.dump
docker compose exec db psql -U postgres -d okf -c "SELECT jsonb_pretty(pgokf.health());"
```

`pgokf-restore` (shipped in the image next to `pgokf-backup`) runs
`pg_restore` as a single transaction that stops on the first error, after
dropping the two kinds of archive entries that cannot be replayed into a
database this image initialized: ParadeDB pg_search's `paradedb` schema, when
that provider was used (the extension does not own its schema, so the archive
tries to recreate it) and, when the
target database is not the one named by `cron.database_name`, the pg_cron
objects. Everything pgokf owns - `CREATE EXTENSION pgokf`, the policy row
(folded into the one the init hook seeded, so the **dumped** policy wins),
bundles, concepts, links, embeddings, history, the audit tables, and sequence
positions - restores exactly as dumped, because pgokf registers all of it with
`pg_extension_config_dump`. The archive's policy replaces whatever
`PGOKF_POLICY` the fresh init applied (that is the point of a restore), so
re-check `allowed_roots` afterwards if the bundles directory moved, and
rebuild the ANN / BM25 indexes if you use them. `pg_restore --disable-triggers`
is the one form that bypasses the policy-row trigger and is not supported for
this catalog.

`pgokf-restore` also rebuilds the runtime indexes the restored catalog uses
(the BM25 index when the policy says `bm25`, the pgvector index when
embeddings were restored), because indexes on extension-owned tables are not
part of a `pg_dump` archive.

The stack's own login roles come back from `.env` through the init hook, so
`roles-<stamp>.sql` is only needed for roles or grants you added by hand.
Read it before applying it: it is a full `pg_dumpall --roles-only`, so it also
carries `ALTER ROLE postgres ... PASSWORD` for the source's superuser and
`CREATE ROLE` statements that error harmlessly for roles that already exist.

---

## Upgrades

Two things must agree after an upgrade: the loaded shared library and the
installed SQL version ([operations.md](operations.md#upgrades)).

1. Back up (above).
2. Point `PGOKF_IMAGE` / `PGOKF_COMPANIONS_IMAGE` at the new version and
   `docker compose pull` (or rebuild locally).
3. `docker compose up -d` - the server restarts on the new image, loading the
   new `.so`; the embed daemon restarts on the new companions image.
4. `docker compose exec db psql -U postgres -d okf -c "ALTER EXTENSION pgokf UPDATE;"`
5. Confirm `SELECT extversion FROM pg_extension WHERE extname='pgokf'` and
   `SELECT pgokf.version()` match.

Between steps 3 and 4 the new library is loaded while the SQL objects are
still the old version, so a companion that calls the catalog in that window
(the `embed` daemon's first watch pass, typically) logs one failed pass such
as `column "bm25_provider" does not exist` and retries on its next interval;
it recovers by itself once step 4 has run.

A PostgreSQL **major** upgrade (e.g. `-pg18` to a future `-pg19`) is a
`pg_dump` / restore or `pg_upgrade` exercise as with any PostgreSQL; the
`PGDATA` path also changes per major.

### Upgrading from 0.1.14 (ParadeDB pg_search) to 0.1.15 or later (pg_textsearch)

The 0.1.15 and later images carry `pg_textsearch` instead of `pg_search`, and the two
cannot coexist in one database (both define the `bm25` index access method).
A 0.1.14 stack has `pg_search` created in `okf` by its init hook, so remove
it **before** switching images - while its library is still loadable - then
bring `pg_textsearch` in on the new image. The catalog's rows are untouched
throughout; only the BM25 index is dropped and rebuilt:

```bash
# 1. On the 0.1.14 image: fall back to native and drop the ParadeDB provider
#    (CASCADE drops the bm25 index it owns; concepts and policy stay).
docker compose exec db psql -U postgres -d okf -c "SELECT pgokf.set_config('search_backend', '\"native\"'::jsonb);"
docker compose exec db psql -U postgres -d okf -c "DROP EXTENSION pg_search CASCADE;"

# 2. Take the compose file from the target release's tag (its shared_preload_libraries
#    names pg_textsearch), point PGOKF_IMAGE / PGOKF_COMPANIONS_IMAGE at it,
#    then pull and restart as in the numbered steps above, including
#    ALTER EXTENSION pgokf UPDATE.

# 3. On the new image: create the new provider (the init hook only runs on
#    an empty data directory), switch back to bm25, and rebuild the index.
docker compose exec db psql -U postgres -d okf -c "CREATE EXTENSION pg_textsearch;"
docker compose exec db psql -U postgres -d okf -c "SELECT pgokf.set_config('search_backend', '\"bm25\"'::jsonb);"
docker compose exec db psql -U postgres -d okf -c "SELECT pgokf.rebuild_search_index();"
docker compose exec db psql -U postgres -d okf -c "SELECT pgokf.search_index_status() -> 'bm25';"
```

`bm25_provider` can stay at its default `auto`, which resolves to
`pg_textsearch` once it is created. Starting a 0.1.15-or-later image with an old
compose file that still preloads `pg_search` fails outright (the library is
not in the image), which is the safe failure: nothing has touched the data
directory yet. To keep ParadeDB instead, build the image with
`--build-arg WITH_PG_SEARCH=1 --build-arg WITH_PG_TEXTSEARCH=0` and leave
`pg_search` in the preload line.

---

## Exposure and TLS

By default PostgreSQL is published on loopback only, and the companions reach
it over the private compose network. To serve clients on other machines,
publish on a private or VPN address (`PGOKF_BIND_ADDR`) and restrict it with
the host firewall. Across an untrusted network, put TLS on the server (mount
`server.crt` / `server.key` and add `-c ssl=on -c ssl_cert_file=... -c
ssl_key_file=...` to the `db` command) and set `OKF_PG_TLS=true` on the
companions, which then verify the certificate against the platform trust
store ([deployment-topologies.md](deployment-topologies.md#beyond-ingestion-the-other-companions)).

The `pgokf.tenant` GUC is a scoping selector, not a hard boundary - if you
serve mutually untrusted tenants, read
[multi-tenancy.md](multi-tenancy.md) before exposing raw SQL.

---

## Troubleshooting

- **`db` never becomes healthy, or refuses to start with "the first
  initialization of this data directory failed".** `docker compose logs db`.
  A failed init hook (an unknown key in `PGOKF_POLICY`, a malformed JSON
  value, a role name that already exists) aborts initialization on purpose and
  leaves a marker so the image will not start the incomplete cluster; fix
  `.env`, empty the data directory, and start again.
- **`embed` logs "endpoint returned a N-dimension vector ... but embedding_dim is M".**
  The policy's `embedding_dim` does not match the model. Set it to N with
  `pgokf.set_config('embedding_dim', 'N')` (as admin, before any vectors are
  stored) and the daemon recovers on its next pass.
- **`register_bundle` says the path is outside `allowed_roots`.** Register
  the container path (`/bundles/<name>`), not the host path.
- **`schedule_refresh` errors about pg_cron.** `pg_cron` is created only in
  the database named by `cron.database_name`, which the stack sets to
  `POSTGRES_DB`; confirm `SELECT extname FROM pg_extension` in that database.
- **Searches warn that the bm25 index is missing.** Run
  `SELECT pgokf.rebuild_search_index();` as admin after loading data.
