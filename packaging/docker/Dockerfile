# syntax=docker/dockerfile:1
#
# pgokf server image.
#
# A drop-in replacement for the official `postgres` image with the pgokf
# extension built and installed, so `CREATE EXTENSION pgokf;` works out of the
# box, plus the optional extensions pgokf lights up at runtime - pgvector
# (semantic / hybrid search), pg_cron (scheduled refresh), and ParadeDB
# pg_search (BM25 ranking) - each toggled by a build argument. CI builds and
# smoke-tests it natively per PostgreSQL major on amd64 AND arm64, and
# publishes a multi-architecture manifest to
# ghcr.io/logicocean/pgokf:<version>-pg<major> on every version tag (so it runs
# unchanged on x86 servers, arm64 servers, and Apple Silicon).
#
# Build (templated for PostgreSQL 15-19 via PG_MAJOR; 19 needs WITH_PG_SEARCH=0
# until ParadeDB publishes a pg19 package and pg_search.sha256 is regenerated):
#   docker build -f packaging/docker/Dockerfile \
#     --build-arg PG_MAJOR=18 \
#     -t pgokf:0.1.14-pg18 .
#
# The build context MUST be the repository root (it copies the source tree;
# .dockerignore keeps target/ and .git out). The build is DRY: it delegates the
# filesystem image to `cargo pgrx package`, then the runtime stage copies that
# tree verbatim onto the stock postgres image.

ARG PG_MAJOR=18
ARG RUST_VERSION=1.96.0
ARG CARGO_PGRX_VERSION=0.19.2

# Optional runtime extensions (1 = install, 0 = omit). pgokf degrades cleanly
# without any of them; see README.md "Optional capabilities".
ARG WITH_PGVECTOR=1
ARG WITH_PG_CRON=1
ARG WITH_PG_SEARCH=1
# ParadeDB pg_search release to install; every (major, codename, arch) .deb of
# this version must have a pinned checksum in packaging/docker/pg_search.sha256.
ARG PG_SEARCH_VERSION=0.25.6
# Extension version for the OCI label; CI passes the value read from
# crates/extension/pgokf.control. Purely informational.
ARG PGOKF_VERSION=""

# --------------------------------------------------------------------------
# Stage 1: build the extension against the matching PostgreSQL server-dev files
# --------------------------------------------------------------------------
FROM postgres:${PG_MAJOR} AS builder

ARG PG_MAJOR
ARG RUST_VERSION
ARG CARGO_PGRX_VERSION

ENV DEBIAN_FRONTEND=noninteractive
ENV CARGO_HOME=/usr/local/cargo
ENV RUSTUP_HOME=/usr/local/rustup
ENV PATH=/usr/local/cargo/bin:${PATH}
# Fail a piped command (the rustup installer below) on the failing stage, not
# only on the last one.
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

# Toolchain + PostgreSQL headers. postgresql-server-dev-N comes from the same
# PGDG apt repo the official postgres image is already configured with.
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        build-essential \
        clang \
        libclang-dev \
        pkg-config \
        curl \
        ca-certificates \
        git \
        "postgresql-server-dev-${PG_MAJOR}"; \
    rm -rf /var/lib/apt/lists/*

# Pinned Rust toolchain, matching rust-toolchain.toml / CI.
RUN set -eux; \
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain "${RUST_VERSION}"

# Pinned cargo-pgrx, matching the workspace pgrx dependency.
RUN cargo install cargo-pgrx --version "${CARGO_PGRX_VERSION}" --locked
RUN cargo pgrx init "--pg${PG_MAJOR}=/usr/lib/postgresql/${PG_MAJOR}/bin/pg_config"

WORKDIR /src
COPY . .

# Produce the filesystem image for this major. Output tree lands at
# /src/target/release/pgokf-pg${PG_MAJOR}/usr/... mirroring the target root.
WORKDIR /src/crates/extension
RUN cargo pgrx package \
        --no-default-features --features "pg${PG_MAJOR}" \
        --pg-config "/usr/lib/postgresql/${PG_MAJOR}/bin/pg_config"

# --------------------------------------------------------------------------
# Stage 2: stock postgres image + pgokf + the optional extensions
# --------------------------------------------------------------------------
FROM postgres:${PG_MAJOR}

ARG PG_MAJOR
ARG WITH_PGVECTOR
ARG WITH_PG_CRON
ARG WITH_PG_SEARCH
ARG PG_SEARCH_VERSION
ARG PGOKF_VERSION

LABEL org.opencontainers.image.title="pgokf" \
      org.opencontainers.image.description="PostgreSQL ${PG_MAJOR} with the pgokf Open Knowledge Format catalog extension (plus pgvector, pg_cron, and pg_search when enabled)" \
      org.opencontainers.image.source="https://github.com/LogicOcean/pgokf" \
      org.opencontainers.image.version="${PGOKF_VERSION}" \
      org.opencontainers.image.licenses="AGPL-3.0-only"

# Optional extensions, installed by one auditable script: pgvector and pg_cron
# from PGDG, pg_search from the pinned ParadeDB release verified against the
# checksum table. The script and table are bind-mounted for the step only, so
# neither ends up in an image layer.
RUN --mount=type=bind,source=packaging/docker,target=/tmp/pgokf-packaging,readonly \
    PG_MAJOR="${PG_MAJOR}" \
    WITH_PGVECTOR="${WITH_PGVECTOR}" \
    WITH_PG_CRON="${WITH_PG_CRON}" \
    WITH_PG_SEARCH="${WITH_PG_SEARCH}" \
    PG_SEARCH_VERSION="${PG_SEARCH_VERSION}" \
    /tmp/pgokf-packaging/install-optional-extensions.sh

# Copy the staged pgrx tree verbatim: .so into the pkglibdir and
# control + sql into the extension sharedir.
COPY --from=builder /src/target/release/pgokf-pg${PG_MAJOR}/usr/ /usr/

# First-init hooks (standard postgres entrypoint rule: they run only when the
# data directory is empty): create pgokf and the available optional extensions,
# then the optional env-driven login roles (20-roles.sh) and catalog policy
# (30-policy.sh). See packaging/docker/README.md.
# --chmod: a hook that lost its executable bit (a Windows checkout, an archive
# export) would be *sourced* by the entrypoint instead of run, with different
# semantics; make the mode part of the image, not of the checkout.
COPY --chmod=0755 packaging/docker/initdb/ /docker-entrypoint-initdb.d/

# Operational tooling (pgokf-backup, pgokf-restore) that rides with the server
# binaries it depends on, and the init-failure guard the image starts through.
COPY --chmod=0755 packaging/docker/bin/ /usr/local/bin/

# Same command surface as the postgres image (`docker run <image> postgres -c
# ...` still works); the guard only refuses to start a half-initialized
# cluster, then hands over to the official entrypoint.
ENTRYPOINT ["pgokf-entrypoint"]
CMD ["postgres"]

# Readiness: the TCP listener accepts connections. pg_isready does not
# authenticate, so no user or database is needed; probing over TCP (not the
# socket) keeps the check red during the entrypoint's socket-only bootstrap
# phase, so `depends_on: condition: service_healthy` waits for the real server
# and the completed init scripts.
HEALTHCHECK --interval=10s --timeout=5s --start-period=60s --retries=6 \
    CMD ["pg_isready", "-h", "127.0.0.1"]
