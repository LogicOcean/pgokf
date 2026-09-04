# pgokf-ingest

The **mountless** OKF ingestion companion for [`pgokf`](../extension).

`pgokf` materializes OKF bundles into a PostgreSQL catalog. The default
"enterprise" topology keeps the bundle bytes in an object store / data lake and
mounts that store into the PostgreSQL host's filesystem so `register_bundle` can
read it. That requires a POSIX mount (Mountpoint for Amazon S3, s3fs-fuse, a CSI
driver, …).

`pgokf-ingest` removes the mount. It is a small, standalone async binary that:

1. connects to an **S3-compatible** object store (AWS S3, MinIO, SeaweedFS,
   Ceph, or GCS/Azure through their S3 surface) using the portable
   [`object_store`](https://crates.io/crates/object_store) crate;
2. lists the objects under a bucket + prefix, downloads each `.md` object, and
   strips the prefix to derive its bundle-relative path;
3. connects to PostgreSQL as a `pgokf_writer`-capable role and calls
   `pgokf.register_bundle_content(name, paths[], contents[])` with the collected
   `(path, bytes)`.

**The extension never performs network I/O.** All object-store access - and all
object-store credentials - live in this companion. PostgreSQL only ever receives
the bytes the companion hands it. Server-side, `register_bundle_content` diffs
the supplied content against the bundle's stored projection, so re-running the
companion is an incremental resync: changed concepts are upserted, and concepts
no longer present are deleted. The bundle is keyed as `content:<name>` with
`source_type = 'content'`.

## Where credentials live

Credentials **never** touch PostgreSQL and are **never** hard-coded.

- **Object store:** supplied via the standard `AWS_ACCESS_KEY_ID` /
  `AWS_SECRET_ACCESS_KEY` environment variables, or an EC2/ECS **instance
  profile / assumed IAM role** when no static keys are set (resolved
  automatically by `object_store`'s `from_env`), or overridden on the command
  line for ad-hoc runs.
- **PostgreSQL:** supplied through the connection string (`--database-url` /
  `OKF_PG_URL`), which authenticates as the ingest account - a login role that
  is a member of `pgokf_writer` (the tier `register_bundle_content` requires).
  It carries no object-store credentials.

## Usage

```
pgokf-ingest \
  --bucket okf-bundles \
  --prefix handbook/ \
  --endpoint http://127.0.0.1:9000 \   # required for MinIO / non-AWS; omit for real S3
  --allow-http \                        # only for a plain-HTTP endpoint
  --bundle-name handbook \
  --database-url "postgresql://okf_ingest@localhost/app"
```

Every flag has an environment-variable equivalent:

| Flag | Env | Meaning |
| --- | --- | --- |
| `--bucket` | `OKF_S3_BUCKET` | Bucket holding the bundle (required) |
| `--prefix` | `OKF_S3_PREFIX` | Key prefix; stripped to derive bundle-relative paths |
| `--endpoint` | `OKF_S3_ENDPOINT` | Object-store endpoint (MinIO/SeaweedFS/Ceph); omit for AWS |
| `--region` | `AWS_REGION` | Region (default `us-east-1`) |
| `--allow-http` | `OKF_S3_ALLOW_HTTP` | Permit a plain-HTTP endpoint |
| `--access-key-id` | `AWS_ACCESS_KEY_ID` | Static key (prefer env / instance profile) |
| `--secret-access-key` | `AWS_SECRET_ACCESS_KEY` | Static secret (prefer env / instance profile) |
| `--database-url` | `OKF_PG_URL` | PostgreSQL connection string for a `pgokf_writer` role (required) |
| `--bundle-name` | `OKF_BUNDLE_NAME` | Bundle name; keyed as `content:<name>` (required) |
| `--concurrency` | `OKF_DOWNLOAD_CONCURRENCY` | Max concurrent object downloads (default 8) |
| `--watch` | `OKF_WATCH` | Run as a daemon: re-list every `--interval` seconds and re-ingest on change (default off) |
| `--interval` | `OKF_WATCH_INTERVAL` | Poll interval in seconds between watch passes, minimum 1 (default 60) |
| `--tls` | `OKF_PG_TLS` | Require a TLS-encrypted link to PostgreSQL (default off) |

### PostgreSQL transport (TLS)

The link to PostgreSQL is plaintext (`NoTls`) by default, which suits a local
socket or a trusted private network. To encrypt it, either pass `--tls` (env
`OKF_PG_TLS=true`) or put `sslmode=require` in the connection string - either one
negotiates a `rustls` TLS session and verifies the server certificate against the
platform trust store:

```bash
pgokf-ingest --tls --database-url "postgresql://okf_ingest@db.internal/app" ...
# or, equivalently:
pgokf-ingest --database-url "postgresql://okf_ingest@db.internal/app?sslmode=require" ...
```

`sslmode=disable`/`prefer` (and an omitted `sslmode`) keep the plaintext default.
Object-store TLS is configured separately (via the endpoint URL and `--allow-http`)
and is unaffected by `--tls`.

On success it prints the sync counts, for example:

```
pgokf-ingest: collected 5 object(s) from s3://okf-bundles/handbook
pgokf-ingest: registered content bundle 'handbook' (bundle_id=1, source_type=content)
	added=5 updated=0 removed=0 unchanged=0 total=5
```

## Scope (v1)

Deliberately minimal but real:

- **One-shot by default, daemon on request.** Without `--watch` the companion
  performs a single sync and exits. With `--watch` it re-lists the object store
  every `--interval` seconds (default 60), re-ingests only when the collected
  content changed (unchanged passes skip the server round-trip), retries a
  failed pass on the next interval, and stops cleanly on SIGINT or SIGTERM
  (`docker stop`). No metrics
  endpoint.
- **Whole-bundle call.** The current object set is sent in a single
  `register_bundle_content` call. This is required for correctness - the server
  computes removals by comparing the supplied set to the stored one, so a
  partial (chunked) call would wrongly delete everything absent from the chunk.
  Downloads are streamed with bounded concurrency; a future large-corpus mode
  could add server-side incremental batching.
- **Optional TLS to PostgreSQL.** Plaintext by default (a private network or
  local socket); `--tls` / `sslmode=require` negotiates a verified `rustls`
  session (see [PostgreSQL transport (TLS)](#postgresql-transport-tls) above).
