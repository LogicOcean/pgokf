# Deployment topologies

`pgokf` separates two things that other catalogs conflate: **where the source
files live** and **where the metadata + search index live**. The metadata and
the search index always live in PostgreSQL. The source files can live either
inside PostgreSQL or in an external object store / data lake. That single choice
— the `store_source` policy key — defines the two supported tiers.

This guide covers both tiers in depth — including feeding the enterprise tier
either through a filesystem mount or **mountless**, with a network companion that
streams the bytes in — how to scale reads horizontally with replicas, how to
isolate tenants, and how to choose. For the mechanics of the
knobs referenced here see [configuration.md](configuration.md); for the trust
boundary around filesystem access see [security.md](security.md).

---

## The one decision: `store_source`

`store_source` is a durable policy key in `pgokf_private.config`, read at sync
time. Set it with `pgokf.set_config('store_source', 'true'::jsonb)` (requires
`pgokf_admin`). It is **not retroactive**: a change takes effect for bundles
synced or refreshed *afterward*, and because `refresh_bundle` re-projects only
changed files, backfilling an existing bundle means re-registering it (see
[operations.md](operations.md#backfilling-stored-sources)).

| | `store_source = true` (**Small**) | `store_source = false` (**Enterprise**, default) |
| --- | --- | --- |
| Source files | Verbatim bytes stored in `pgokf.concept_source` (lz4-compressed where the build supports it) | Left in their external store; PostgreSQL never keeps a copy |
| PostgreSQL holds | Metadata, search index, **and** originals | Metadata and search index only |
| `get_concept_source` | Returns the stored bytes | Errors: "no source is stored… store_source disabled" |
| `export_sources` | Reconstructs the files from the DB | Not usable (nothing stored to export) |
| Backup captures sources? | Yes — `pg_dump` carries everything | No — back up the object store separately |
| Best when | Self-contained install, small/medium corpus, portability matters | Large corpus, files already governed in a lake, DB stays lean |

Everything else — `register_bundle`, `concept_search`, `concept_neighbors`,
`export_parquet` — behaves identically in both tiers. Only source *retrieval*
and what a backup captures differ.

---

## Small tier: PostgreSQL-only, self-contained

Enable `store_source` and the originals travel with the catalog. The database is
the whole system: one thing to deploy, one thing to back up, one thing to
replicate.

```sql
SELECT pgokf.set_config('store_source', 'true'::jsonb);
-- then register (or re-register) bundles so their sources are captured
SELECT * FROM pgokf.register_bundle('/srv/okf/knowledge', 'knowledge');
```

**Why choose it**

- **Portability.** `pg_dump` produces a single artifact that contains metadata,
  search index, *and* the source files. Restore it anywhere and the catalog is
  complete — no second system to rehydrate.
- **Operational simplicity.** No mount, no bucket credentials, no lake to keep
  in sync. Backup and replication (below) cover the sources for free because
  they are just table data.
- **Consistency.** The stored bytes hash to `pgokf.concepts.file_hash`, so the
  copy in the database is provably the file that was indexed.

**Backup / restore.** Ordinary PostgreSQL tooling. `pg_dump` captures
`pgokf.concept_source` along with the rest of the catalog; a restore yields a
byte-identical corpus. See [operations.md](operations.md#backup-and-restore).

**Cost.** The source bytes live in `pgokf.concept_source`, TOAST-compressed with
lz4 where the server build offers it (pglz otherwise). Small and medium corpora
absorb this comfortably; very large corpora are the reason the enterprise tier
exists.

**When to pick it:** a self-contained knowledge base, an appliance or edge
install, a demo, a corpus that fits comfortably in the database, or anywhere you
want a single restorable artifact.

---

## Enterprise tier: files in an object store / data lake

Leave `store_source` at its default `false`. The source files stay where they
already are — an S3-compatible bucket, a data lake — **mounted into the
filesystem** so the PostgreSQL backend can read them at sync time. PostgreSQL
holds only metadata and the search index and stays lean; the lake remains the
system of record for the bytes.

`pgokf` reads bundles through the normal filesystem, so the object store must be
exposed as a POSIX mount. Use a purpose-built mount rather than copying files
onto the DB host:

- **[Mountpoint for Amazon S3](https://github.com/awslabs/mountpoint-s3)** on the
  DB host, or its **CSI driver** for Kubernetes-hosted PostgreSQL.
- **[s3fs-fuse](https://github.com/s3fs-fuse/s3fs-fuse)** for any
  S3-compatible endpoint (AWS, MinIO, Ceph, GCS via its S3 interface).

Then point `allowed_roots` at the mountpoint so registration is confined to the
lake:

```sql
SELECT pgokf.set_config('allowed_roots', '["/mnt/okf"]'::jsonb);
SELECT * FROM pgokf.register_bundle('/mnt/okf/knowledge', 'knowledge');
```

> **Use IAM roles, not static keys.** Grant the DB host (or pod) an instance /
> workload IAM role and let the mount driver assume it. Do not bake long-lived
> access keys into the mount config or the environment. This keeps credentials
> out of the database, out of backups, and out of `pgokf_private.config` — the
> extension never sees them. `pgokf`'s CLAUDE-level rule is the same one to
> apply here: no static secrets on the box.

### Verified example: MinIO + s3fs

This topology was verified end to end against a real MinIO bucket mounted with
s3fs and registered through `pgokf`. The shape:

```bash
# 1. A bucket in MinIO (or any S3-compatible store) holds the OKF bundle.
#    Objects laid out exactly like the directory tree:
#      knowledge/index.md
#      knowledge/services/postgresql.md
#      knowledge/runbooks/database-failover.md   ...

# 2. Provide the endpoint's credentials to s3fs OUT OF BAND. For MinIO in a lab
#    this is a passwd file (0600); in production prefer an IAM role and an
#    s3fs build/driver that sources temporary credentials, so no static key
#    lands on disk.
install -m 600 /dev/null /etc/passwd-s3fs
printf '%s' "$MINIO_ACCESS_KEY:$MINIO_SECRET_KEY" > /etc/passwd-s3fs   # lab only

# 3. Mount the bucket where the postgres OS user can read it.
mkdir -p /mnt/okf
s3fs okf-bundles /mnt/okf \
     -o passwd_file=/etc/passwd-s3fs \
     -o url=http://minio.internal:9000 \
     -o use_path_request_style \
     -o umask=0022,uid=$(id -u postgres),gid=$(id -g postgres)
```

```sql
-- 4. Confine registration to the mount, then register the bundle from it.
SELECT pgokf.set_config('allowed_roots', '["/mnt/okf"]'::jsonb);
SELECT * FROM pgokf.register_bundle('/mnt/okf/knowledge', 'knowledge');
```

From here search, graph, and Parquet export work identically to a local bundle —
the backend simply reads the concept files through the FUSE mount. Because
`store_source` stays `false`, no source bytes are copied into PostgreSQL; the
bucket remains the single source of truth for the files.

**Operational notes for a lake mount**

- **`allowed_roots` must resolve through the mount.** Containment is checked
  after resolving symlinks on both sides, so the requested path and the mount
  must canonicalize into the same real directory — see
  [security.md](security.md#allowed_roots-containment).
- **Mount availability is a dependency.** If the FUSE mount is down, `register`
  / `refresh` for bundles under it fail (the files are unreadable); already-
  indexed metadata and search are unaffected because they live in PostgreSQL.
- **`refresh_bundle` re-lists the mount.** Object-store latency shows up as
  refresh latency. Schedule refreshes accordingly (see
  [operations.md](operations.md#refresh-scheduling)).
- **Respect the file-size and count ceilings.** `pgokf.max_file_bytes` and
  `pgokf.max_bundle_files` bound what one sync will ingest from the lake; see
  [configuration.md](configuration.md#gucs-resource-ceilings).

**When to pick it:** a large corpus, files already curated and governed in a
lake, a policy that the DB must not hold document bytes, or many bundles sharing
one governed store.

---

## Enterprise tier, mountless: the ingestion companion

The mount above puts the object store *behind the filesystem* so the PostgreSQL
backend can read it. That is not always possible or desirable: a managed
PostgreSQL (RDS, Cloud SQL, a Kubernetes operator) may not let you attach a FUSE
mount to the database host at all, and a mount couples database availability to
mount availability. The **mountless** variant of the enterprise tier removes the
mount entirely.

Instead of the backend reading files, a small standalone companion —
[`pgokf-ingest`](https://github.com/LogicOcean/pgokf/tree/main/crates/pgokf-ingest) — reads the object store over the
network and streams the bytes into PostgreSQL through a new writer-tier
function:

```
pgokf.register_bundle_content(name text, paths text[], contents bytea[],
                              options jsonb DEFAULT '{}')
    RETURNS pgokf.bundle_sync_result
```

The extension still performs **no network I/O**. It receives only the bytes the
companion hands it, runs them through the identical classify/parse/upsert/project
pipeline `register_bundle` uses, and records the bundle with
`source_type = 'content'` under the synthetic key `content:<name>`. A content
bundle is diffed against its stored projection exactly like a filesystem bundle,
so **re-running the companion is an incremental resync** — changed concepts are
upserted and removed ones deleted. (A content bundle has no on-disk root, so
`pgokf.refresh_bundle` on it raises `22023`: you resync by calling
`register_bundle_content` again. `unregister_bundle`, search, graph, and
`export_parquet` all behave identically to any other bundle.)

### Mount vs. mountless — which enterprise variant

| | **Mounted** (`register_bundle` over a FUSE mount) | **Mountless** (`register_bundle_content` via the companion) |
| --- | --- | --- |
| Where object-store I/O happens | The PostgreSQL backend, through the mount | The companion process, over the network |
| Needs a FUSE mount on the DB host | Yes | **No** |
| Works with managed PostgreSQL (RDS/Cloud SQL) | Rarely (no host mount) | **Yes** |
| Object-store credentials | On the DB host (mount driver / IAM role) | On the companion only — never near the DB |
| Availability coupling | DB register/refresh depends on the mount | DB is decoupled; the companion runs anywhere |
| Incremental sync | `refresh_bundle` re-lists the mount | Re-run the companion; server-side diff |

### Credentials never touch PostgreSQL

This is the whole point of the split. The object-store credentials live in the
**companion's** environment — the standard `AWS_ACCESS_KEY_ID` /
`AWS_SECRET_ACCESS_KEY`, or, preferably, an EC2/ECS **instance profile / IAM
role** that the companion assumes with no static keys at all. PostgreSQL is
reached separately, through a connection string for a login role that is a member
of **`pgokf_writer`** (the ingest tier `register_bundle_content` requires). That
account carries no object-store credentials. Neither secret ever lands in
`pgokf_private.config`, in a backup, or in the extension's view of the world.

### Concrete example: MinIO bucket → managed PostgreSQL, no mount

```bash
# The bucket okf-bundles holds the OKF bundle under the handbook/ prefix:
#   handbook/attester.md
#   handbook/computation.md
#   handbook/rich-concept.md   ...

# Object-store credentials live here, in the companion's environment (or, in
# production, an attached IAM role so there is no static key at all).
export AWS_ACCESS_KEY_ID=…                # your object-store access key id
export AWS_SECRET_ACCESS_KEY=…            # out of band; never sent to PostgreSQL

# PostgreSQL is reached as a pgokf_writer login role, separately.
export OKF_PG_URL="postgresql://okf_ingest@db.internal/app"

pgokf-ingest \
  --bucket okf-bundles \
  --prefix handbook/ \
  --endpoint http://minio.internal:9000 \  # required for MinIO; omit for real AWS S3
  --allow-http \                            # only for a plain-HTTP endpoint
  --bundle-name handbook
```

```text
pgokf-ingest: collected 5 object(s) from s3://okf-bundles/handbook
pgokf-ingest: registered content bundle 'handbook' (bundle_id=1, source_type=content)
	added=5 updated=0 removed=0 unchanged=0 total=5
```

From here `concept_search`, `concept_neighbors`, `list_bundles`, and
`export_parquet` work exactly as for a mounted or local bundle — the catalog
cannot tell how the bytes arrived, only that `source_type = 'content'`. Because
`store_source` defaults to `false`, no source bytes are retained in PostgreSQL;
the bucket remains the source of truth. Set `store_source = true` first if you
also want the originals captured in `pgokf.concept_source` (the small tier —
useful even here for a self-contained backup). Re-run the companion whenever the
bucket changes; the server-side diff makes it incremental. See the companion's
[README](https://github.com/LogicOcean/pgokf/blob/main/crates/pgokf-ingest/README.md) for the full flag/environment
reference and its v1 scope (one-shot sync; whole-bundle call for correct
removals; `NoTls` transport, so front a public endpoint with TLS).

---

## Scaling reads with replicas

Search and graph traversal are read-only. `concept_search` and
`concept_neighbors` are declared `stable, parallel_safe`, and the catalog tables
carry the GIN and btree indexes they need
(`concepts_body_tsv_gin`, `concepts_tags_gin`, `concept_metadata_value_gin`,
`concepts_type_idx`, `concepts_path_idx`, `links_target_idx`). That makes them a
natural fit for **physical streaming replicas**: point read traffic at hot
standbys and scale recall horizontally.

```text
                 register / refresh (writes)
                          │
                          ▼
                 ┌───────────────┐
   search /      │   PRIMARY     │  ── WAL ──► ┌───────────┐  search / graph
   graph  ◄──────┤  (writable)   │            │ replica 1 ├──────────►
                 └───────┬───────┘  ── WAL ──► └───────────┘
                         │                     ┌───────────┐  search / graph
                         └───── WAL ──────────►│ replica 2 ├──────────►
                                               └───────────┘
```

Guidance:

- **Writes go to the primary.** `register_bundle`, `refresh_bundle`,
  `unregister_bundle`, `set_config`, `reset_config`, `export_parquet`, and
  `export_sources` are `VOLATILE` and must run on a writable primary; they will
  raise on a read-only standby.
- **Grant `pgokf_reader` on the replicas.** The roles are cluster-wide and
  replicate with the catalog, so a reader that can search the primary can search
  a standby.
- **Both tiers replicate the metadata and index.** In the **small** tier the
  source bytes in `pgokf.concept_source` replicate too, so `get_concept_source`
  and `export_sources` work on a standby. In the **enterprise** tier standbys
  read sources through the same lake mount as the primary (mount them the same
  way), or simply do not store sources on standbys at all.

### Failover and consistency caveats

- **Replication lag is real.** A concept registered on the primary is visible on
  a standby only after its WAL replays. If a workflow registers then immediately
  searches, either target the primary for that read or tolerate the lag.
- **`register`/`refresh` serialize per bundle** via an advisory lock keyed on the
  bundle's canonical path, so two concurrent syncs of the same bundle cannot
  interleave — but that lock lives on the primary and does not coordinate across
  a failover mid-sync. After a failover, re-run any sync that was in flight;
  `refresh_bundle` is idempotent (unchanged files report `unchanged`).
- **GUC ceilings are per-server.** `pgokf.max_file_bytes` and friends come from
  each server's own `postgresql.conf`. Keep the ceilings identical across
  primary and standbys, or a promoted standby will enforce different limits than
  the old primary did. Put them in a shared config-management template.

---

## Multi-tenant isolation with RLS

`pgokf` does not ship row-level security policies. When one catalog serves
multiple tenants and you need hard isolation between them, the mechanism is
PostgreSQL **Row-Level Security (RLS)** layered on top of the catalog tables —
you build it; it is not built in yet.

The shape: carry a tenant discriminator per bundle (the `bundles.options` jsonb
is stored verbatim for exactly this kind of producer metadata), and gate the
reader-facing tables on it.

```sql
-- SKETCH — you own and must review this; it is not part of the extension.

-- 1. Tag each bundle with its tenant at registration time.
SELECT pgokf.register_bundle('/srv/okf/acme', 'acme',
                             '{"tenant": "acme"}'::jsonb);

-- 2. Enable RLS on the reader-facing tables and gate on the tenant of the
--    owning bundle. Concepts join to their bundle's options->>'tenant'.
ALTER TABLE pgokf.concepts ENABLE ROW LEVEL SECURITY;

CREATE POLICY concepts_tenant_isolation ON pgokf.concepts
    USING (
        EXISTS (
            SELECT 1 FROM pgokf.bundles b
            WHERE b.id = concepts.bundle_id
              AND b.options->>'tenant'
                  = current_setting('pgokf.tenant', true)
        )
    );

-- 3. A per-tenant connection sets its tenant, and can see only its rows.
SET pgokf.tenant = 'acme';
```

Caveats to weigh before relying on this:

- **`concept_search` / `concept_neighbors` are `SECURITY INVOKER`** functions
  that query these tables as the calling role, so RLS policies on the tables *do*
  apply to them — but you must confirm this against every code path you expose,
  and test it, before treating it as an isolation boundary.
- **`bundles.options` is untrusted producer input.** Do not let a tenant set its
  own discriminator; assign it administratively at registration.
- **Admin functions bypass tenant scoping.** `pgokf_admin` operations
  (register/refresh/config/export) are cross-tenant by nature — keep admin a
  separate, trusted role, per [security.md](security.md#roles-and-least-privilege).
- **This is a sketch, not a shipped feature.** Review, harden, and test it as
  security-critical code you own. Treat a future first-class multi-tenant mode as
  not-yet-available.

For the near term, the simplest hard isolation is still **one database (or one
cluster) per tenant** — no shared tables, nothing to leak — at the cost of more
instances to operate.

---

## Choosing a topology

Answer these in order:

1. **Must the database hold the document bytes, or should the lake?**
   Small corpus / portability / single restorable artifact → **small tier**
   (`store_source = true`). Large corpus / files already governed in a lake / DB
   must stay lean → **enterprise tier** (`store_source = false`).
2. **How much read traffic?** Beyond one server's headroom on broad searches →
   add **streaming replicas** and route reads to them; keep GUC ceilings
   identical across all servers.
3. **One tenant or many, and how hard is the isolation requirement?**
   Soft / trusted → single catalog. Hard multi-tenant now → **database/cluster
   per tenant**. Hard multi-tenant in one catalog → **RLS you build and own**
   (sketch above), tested as a security boundary.

Then follow [operations.md](operations.md) for running whichever you pick, and
[configuration.md](configuration.md) for the exact knobs.
