# pgokf Docker image

A drop-in replacement for the official [`postgres`](https://hub.docker.com/_/postgres)
image with the **pgokf** extension pre-built and installed, so
`CREATE EXTENSION pgokf;` works out of the box.

> **Published on release.** CI
> ([`.github/workflows/packages.yml`](../../.github/workflows/packages.yml))
> builds one image per supported PostgreSQL major and smoke-tests it (start the
> container, register the sample bundle, assert concept rows materialize). On a
> version tag it then pushes the smoke-tested image to
> `ghcr.io/logicocean/pgokf:<version>-pg<major>`. Between releases there is
> nothing to pull, so use the [build](#build) command below.

## Image tags

One image per PostgreSQL major (15-19), selected at build time via the
`PG_MAJOR` build argument. These are the local tag names produced by the build
below; `0.1.13` is the extension version, read from
`crates/extension/pgokf.control` - the single source of truth that CI and
`packaging/deb/build-deb.sh` both resolve at build time.

| Tag             | PostgreSQL                    |
| --------------- | ----------------------------- |
| `0.1.13-pg15`   | 15                            |
| `0.1.13-pg16`   | 16                            |
| `0.1.13-pg17`   | 17                            |
| `0.1.13-pg18`   | 18 (the `PG_MAJOR` default)   |
| `0.1.13-pg19`   | 19 (once PGDG ships packages) |

## Build

The build context **must be the repository root** - the Dockerfile copies the
whole source tree:

```bash
docker build -f packaging/docker/Dockerfile \
  --build-arg PG_MAJOR=18 \
  -t pgokf:0.1.13-pg18 .
```

The tag is a purely local name. CI builds the same image under the
`ghcr.io/logicocean/pgokf:<version>-pg<major>` name it would publish under, but
that reference is not resolvable until a release actually pushes it.

Pinned build arguments (override only deliberately):

| Arg                   | Default   | Meaning                                  |
| --------------------- | --------- | ---------------------------------------- |
| `PG_MAJOR`            | `18`      | PostgreSQL major (15-19)                 |
| `RUST_VERSION`        | `1.96.0`  | matches `rust-toolchain.toml`            |
| `CARGO_PGRX_VERSION`  | `0.19.2`  | matches the workspace `pgrx` dependency  |

The image is multi-stage: stage 1 installs the pinned Rust toolchain and
`cargo-pgrx`, then runs `cargo pgrx package` against `postgresql-server-dev-N`;
stage 2 is the stock `postgres:N` image with the staged tree copied verbatim
into `/usr`. Only the built `.so`, `.control`, and `.sql` survive into the
final image - no Rust toolchain, no source.

## Run

Run the image you just built:

```bash
docker run --rm -e POSTGRES_PASSWORD=postgres \
  pgokf:0.1.13-pg18
```

The extension is created automatically in the default `postgres` database on
first initialization (see `initdb/10-create-extension.sql`). Verify:

```bash
docker exec -it <container> \
  psql -U postgres -c "SELECT extname, extversion FROM pg_extension WHERE extname='pgokf';"
```

Registering an OKF bundle additionally requires a **server-readable absolute
bundle path** (mount it into the container) and membership in `pgokf_admin`;
see the project [README](../../README.md).

## compose

```yaml
services:
  db:
    # Locally built (see Build above); nothing is pulled.
    image: pgokf:0.1.13-pg18
    environment:
      POSTGRES_PASSWORD: postgres
    ports:
      - "5432:5432"
    volumes:
      # Mount an OKF bundle where the server process can read it.
      - ./examples/sample-bundle:/bundles/sample:ro
```
