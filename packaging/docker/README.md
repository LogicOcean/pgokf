# pgokf Docker images

Two images are built from this directory, both from the **repository root**
as the build context (`.dockerignore` keeps `target/` and `.git/` out):

| Image | Dockerfile | Contents |
| ----- | ---------- | -------- |
| `ghcr.io/logicocean/pgokf:<version>-pg<major>` | [`Dockerfile`](Dockerfile) | The official `postgres:<major>` image plus **pgokf**, and the optional extensions it lights up: **pgvector** (semantic / hybrid search), **pg_cron** (scheduled refresh), and a BM25 provider - **Tiger Data pg_textsearch** (PostgreSQL license) on the 17 and 18 images by default, or **ParadeDB pg_search** when opted in. First-init hooks create the extensions, least-privilege login roles, and the catalog policy from the environment. Ships the `pgokf-backup` / `pgokf-restore` tools. |
| `ghcr.io/logicocean/pgokf-companions:<version>` | [`Dockerfile.companions`](Dockerfile.companions) | The three network companions - `pgokf-ingest`, `pgokf-embed`, `pgokf-mcp` - in one small non-root image. |

Both are **multi-architecture** (`linux/amd64` + `linux/arm64`): CI
([`.github/workflows/packages.yml`](../../.github/workflows/packages.yml))
builds each architecture natively on its own runner, smoke-tests it there
with the scripts in this directory, and on a version tag re-exports the same
cached build by digest and merges the two into one manifest per tag - so the
same tag runs on x86 servers, arm64 servers, and Apple Silicon. Between releases there is nothing new to pull; build
locally as below. `0.1.15` in the examples is the extension version, read from
`crates/extension/pgokf.control` - the single source of truth CI and
`packaging/deb/build-deb.sh` both resolve at build time.

A ready-to-run production stack (server, embedder daemon, backups, optional
ingestion and MCP services) lives in [`deploy/compose/`](../../deploy/compose)
and is documented in [docs/compose-deployment.md](../../docs/compose-deployment.md).

## Server image

### Tags

One image per PostgreSQL major (15-19), selected at build time via `PG_MAJOR`:

| Tag             | PostgreSQL                    |
| --------------- | ----------------------------- |
| `0.1.15-pg15`   | 15 (no BM25 provider: `pg_textsearch` ships for 17 and 18 only) |
| `0.1.15-pg16`   | 16 (no BM25 provider)         |
| `0.1.15-pg17`   | 17 (+ `pg_textsearch`)        |
| `0.1.15-pg18`   | 18 (+ `pg_textsearch`; the `PG_MAJOR` default) |
| `0.1.15-pg19`   | 19 (once PGDG ships packages; no BM25 provider until Tiger Data publishes a pg19 package) |

### Build

```bash
docker build -f packaging/docker/Dockerfile \
  --build-arg PG_MAJOR=18 \
  --build-arg PGOKF_VERSION="$(sed -n "s/^default_version *= *'\([^']*\)'.*/\1/p" crates/extension/pgokf.control)" \
  -t pgokf:0.1.15-pg18 .
```

The build runs natively for the daemon's architecture. To build for an arm64
host from an x86 workstation (or vice versa), point `docker` at a daemon of
that architecture (`docker --context <name> build ...`) or use
`docker buildx build --platform linux/arm64` with emulation.

Build arguments (override only deliberately):

| Arg                   | Default   | Meaning                                                                 |
| --------------------- | --------- | ----------------------------------------------------------------------- |
| `PG_MAJOR`            | `18`      | PostgreSQL major (15-19)                                                |
| `RUST_VERSION`        | `1.96.0`  | matches `rust-toolchain.toml`                                           |
| `CARGO_PGRX_VERSION`  | `0.19.2`  | matches the workspace `pgrx` dependency                                 |
| `WITH_PGVECTOR`       | `1`       | install `postgresql-<major>-pgvector` from PGDG                         |
| `WITH_PG_CRON`        | `1`       | install `postgresql-<major>-cron` from PGDG                             |
| `WITH_PG_TEXTSEARCH`  | `auto`    | install Tiger Data `pg_textsearch` (`auto` = where a package exists: 17, 18; `1` = required; `0` = omit) |
| `PG_TEXTSEARCH_VERSION` | `1.4.0` | pg_textsearch release; must have entries in `pg_textsearch.sha256`      |
| `WITH_PG_SEARCH`      | `0`       | install ParadeDB `pg_search` instead (requires `WITH_PGVECTOR=1` and `WITH_PG_TEXTSEARCH=0`: both define the `bm25` access method) |
| `PG_SEARCH_VERSION`   | `0.25.6`  | ParadeDB release; must have entries in `pg_search.sha256`               |
| `PGOKF_VERSION`       | `""`      | value of the `org.opencontainers.image.version` label (informational)   |

The image is multi-stage: stage 1 installs the pinned Rust toolchain and
`cargo-pgrx`, then runs `cargo pgrx package` against
`postgresql-server-dev-N`; stage 2 is the stock `postgres:N` image with the
optional extensions installed by
[`install-optional-extensions.sh`](install-optional-extensions.sh) and the
staged pgrx tree copied verbatim into `/usr`. Only the built `.so`, `.control`,
and `.sql` files, the extension packages, the init hooks, and the
`pgokf-backup` / `pgokf-restore` tools survive into the final image - no Rust
toolchain, no source, no download tooling.

**BM25 provider supply chain.** pgvector and pg_cron come from the PGDG apt
repository the base image is already configured with. Neither BM25 provider
is in PGDG; the installer downloads the exact package for the image's
PostgreSQL major and architecture from the pinned upstream GitHub release
(`pg_textsearch` ships a `.deb` inside a zip, fetched and verified by
[`fetch-pg-textsearch.sh`](fetch-pg-textsearch.sh), which CI's test workflow
also uses; `pg_search` a `.deb` per Debian codename) and refuses to install it
unless its SHA256 matches the entry in
[`pg_textsearch.sha256`](pg_textsearch.sha256) /
[`pg_search.sha256`](pg_search.sha256). `WITH_PG_TEXTSEARCH=auto` skips
quietly when the table has no entry for the major, so CI builds the 17 and 18
images with `WITH_PG_TEXTSEARCH=1` and the smoke test refuses a 17/18 image
without a provider. The `pg_textsearch` package carries no copyright file, so
its PostgreSQL-license notice
([`pg_textsearch-LICENSE`](pg_textsearch-LICENSE)) is installed as
`/usr/share/doc/pg-textsearch-postgresql-<major>/copyright`; pgvector and
pg_cron bring their own from PGDG. Bumping a provider version (or, for
pg_search, a base-image codename change) therefore requires regenerating the
matching table:

```bash
packaging/docker/update-pg-textsearch-checksums.sh 1.4.0
packaging/docker/update-pg-search-checksums.sh 0.25.6
```

### Run

```bash
docker run --rm -e POSTGRES_PASSWORD=postgres \
  pgokf:0.1.15-pg18 \
  postgres -c shared_preload_libraries=pgokf,pg_cron,pg_textsearch
```

On first initialization the hooks in [`initdb/`](initdb) run, in order:

1. `10-create-extension.sql` creates `pgokf` in the default database, plus
   `vector` whenever the image carries it, and the preloaded BM25 provider
   (`pg_textsearch`, else `pg_search`) / `pg_cron` when they are in
   `shared_preload_libraries` (pg_cron only in the database named by
   `cron.database_name`, default `postgres`). Preloading `pgokf` itself is
   what makes its `pgokf.*` GUC ceilings settable on the command line.
2. `20-roles.sh` creates one login role per pgokf tier for each password
   supplied, so a deployment starts with least-privilege accounts:

   | Variable                              | Creates                                   |
   | ------------------------------------- | ----------------------------------------- |
   | `PGOKF_ADMIN_PASSWORD`  (+ `_ROLE`)   | `okf_admin`, member of `pgokf_admin`      |
   | `PGOKF_WRITER_PASSWORD` (+ `_ROLE`)   | `okf_writer`, member of `pgokf_writer`    |
   | `PGOKF_READER_PASSWORD` (+ `_ROLE`)   | `okf_reader`, member of `pgokf_reader`    |

   Each password may instead be supplied as a file through
   `PGOKF_*_PASSWORD_FILE` (compose secrets). Passwords are read by psql from
   the environment (`\getenv`), never placed on a command line, and the one
   statement that carries them is excluded from the server log even on
   failure. A hook that fails leaves a marker in the data directory, and the
   image's entrypoint then refuses to start the half-initialized cluster
   until the directory is emptied.
3. `30-policy.sh` applies `PGOKF_POLICY`, a JSON object of
   `pgokf.set_config` keys, for example
   `{"embedding_dim": 1024, "store_source": true, "allowed_roots": ["/bundles"]}`.
   Every pair goes through the validating public function; an unknown key
   aborts initialization loudly.

Verify:

```bash
docker exec -it <container> \
  psql -U postgres -c "SELECT extname, extversion FROM pg_extension ORDER BY 1;"
```

Registering an OKF bundle additionally requires a **server-readable absolute
bundle path** (mount it into the container, read-only) and membership in
`pgokf_writer`; see the project [README](../../README.md).

The image declares a `HEALTHCHECK` (`pg_isready` over TCP for the configured
superuser and database), so `depends_on: condition: service_healthy` waits for
the real server and the completed init hooks, not for the entrypoint's
socket-only bootstrap phase.

### Backups and restore

`pgokf-backup` (in `/usr/local/bin`) writes a verified `pg_dump` custom-format
archive plus a `pg_dumpall --roles-only` file and checksums into
`PGOKF_BACKUP_DIR`, pruning artifacts older than
`PGOKF_BACKUP_RETENTION_DAYS`. `pgokf-restore <archive>` replays one into the
target database as a single transaction that stops on the first error,
skipping the entries a database this image initialized cannot replay
(ParadeDB pg_search's unowned `paradedb` schema when that provider was used,
and pg_cron's objects when the target is not `cron.database_name`). Both use the standard libpq variables to connect:

```bash
# on the server's network (here the compose network), as the superuser
docker run --rm --network pgokf-net \
  -e PGHOST=pgokf-db -e PGUSER=postgres -e PGPASSWORD=... -e PGDATABASE=okf \
  -v /srv/pgokf/backups:/backups pgokf:0.1.15-pg18 pgokf-backup
docker run --rm --network pgokf-net \
  -e PGHOST=pgokf-db -e PGUSER=postgres -e PGPASSWORD=... -e PGDATABASE=okf \
  -v /srv/pgokf/backups:/backups pgokf:0.1.15-pg18 pgokf-restore /backups/okf-<stamp>.dump
```

Dump as a superuser (or a role with `pg_read_all_data` and `BYPASSRLS`); the
catalog tables carry row-level security and the extension owns their
sequences, so a lesser role fails outright rather than dumping less.

The archive carries the catalog's rows (pgokf registers its tables with
`pg_extension_config_dump`), and the smoke test below restores one into a
fresh container to prove it.

### Smoke test

The same script CI runs, usable against any daemon (the sample bundle is
copied in with `docker cp`, so no daemon-side path is needed):

```bash
packaging/docker/smoke-test.sh pgokf:0.1.15-pg18            # local daemon
DOCKER="docker --context <remote>" packaging/docker/smoke-test.sh pgokf:0.1.15-pg18
```

It preloads every optional extension, applies env-driven roles and policy,
registers the sample bundle, checks `health()`, searches with BM25 as the
non-superuser reader role, runs `pgokf-backup`, and restores the archive into
a second fresh container with `pgokf-restore`.

## Companions image

```bash
docker build -f packaging/docker/Dockerfile.companions -t pgokf-companions:0.1.15 .
docker run --rm pgokf-companions:0.1.15 pgokf-embed --help
packaging/docker/smoke-test-companions.sh pgokf-companions:0.1.15
```

The image has no entrypoint: name the binary as the command. It runs as an
unprivileged user (uid 10001) and carries only the three binaries and a CA
bundle for TLS. Configuration is entirely by environment variables (see each
companion's README under [`crates/`](../../crates)); the compose stack wires
them up.

## compose

The minimal shape (the full stack is in [`deploy/compose/`](../../deploy/compose)):

```yaml
services:
  db:
    image: ghcr.io/logicocean/pgokf:0.1.15-pg18
    command: ["postgres", "-c", "shared_preload_libraries=pgokf,pg_cron,pg_textsearch"]
    environment:
      POSTGRES_PASSWORD: postgres
      PGOKF_POLICY: '{"allowed_roots": ["/bundles"]}'
    ports:
      - "127.0.0.1:5432:5432"
    volumes:
      # Mount an OKF bundle where the server process can read it.
      - ./examples/sample-bundle:/bundles/sample:ro
```
