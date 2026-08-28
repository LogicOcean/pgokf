# pgokf Docker image

A drop-in replacement for the official [`postgres`](https://hub.docker.com/_/postgres)
image with the **pgokf** extension pre-built and installed, so
`CREATE EXTENSION pgokf;` works out of the box. Published to
`ghcr.io/logicocean/pgokf`.

## Supported tags

One image per PostgreSQL major (15-19), selected at build time via the
`PG_MAJOR` build argument:

| Tag                  | PostgreSQL |
| -------------------- | ---------- |
| `0.1.3-pg15`         | 15         |
| `0.1.3-pg16`         | 16         |
| `0.1.3-pg17`         | 17         |
| `0.1.3-pg18`, `latest` | 18       |
| `0.1.3-pg19`         | 19 (once PGDG ships packages) |

## Build

The build context **must be the repository root** — the Dockerfile copies the
whole source tree:

```bash
docker build -f packaging/docker/Dockerfile \
  --build-arg PG_MAJOR=18 \
  -t ghcr.io/logicocean/pgokf:0.1.3-pg18 .
```

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
final image — no Rust toolchain, no source.

## Run

```bash
docker run --rm -e POSTGRES_PASSWORD=postgres \
  ghcr.io/logicocean/pgokf:0.1.3-pg18
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
    image: ghcr.io/logicocean/pgokf:0.1.3-pg18
    environment:
      POSTGRES_PASSWORD: postgres
    ports:
      - "5432:5432"
    volumes:
      # Mount an OKF bundle where the server process can read it.
      - ./examples/sample-bundle:/bundles/sample:ro
```
