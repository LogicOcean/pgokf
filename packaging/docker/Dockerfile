# syntax=docker/dockerfile:1
#
# pgokf development image.
#
# A drop-in replacement for the official `postgres` image with the pgokf
# extension already built and installed, so `CREATE EXTENSION pgokf;` works
# out of the box. Intended for ghcr.io/logicocean/pgokf.
#
# Build (templated for PostgreSQL 15-19 via PG_MAJOR):
#   docker build -f packaging/docker/Dockerfile \
#     --build-arg PG_MAJOR=18 \
#     -t ghcr.io/logicocean/pgokf:0.1.3-pg18 .
#
# The build context MUST be the repository root (it copies the whole source
# tree). The build is DRY: it delegates the filesystem image to
# `cargo pgrx package`, then the runtime stage copies that tree verbatim onto
# the stock postgres image.

ARG PG_MAJOR=18
ARG RUST_VERSION=1.96.0
ARG CARGO_PGRX_VERSION=0.19.2

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
RUN set -eux; \
    cd crates/extension; \
    cargo pgrx package \
        --no-default-features --features "pg${PG_MAJOR}" \
        --pg-config "/usr/lib/postgresql/${PG_MAJOR}/bin/pg_config"

# --------------------------------------------------------------------------
# Stage 2: stock postgres image + the installed extension
# --------------------------------------------------------------------------
FROM postgres:${PG_MAJOR}

ARG PG_MAJOR
LABEL org.opencontainers.image.title="pgokf" \
      org.opencontainers.image.description="PostgreSQL ${PG_MAJOR} with the pgokf Open Knowledge Format catalog extension" \
      org.opencontainers.image.source="https://github.com/LogicOcean/pgokf" \
      org.opencontainers.image.licenses="MIT"

# Copy the staged pgrx tree verbatim: .so into the pkglibdir and
# control + sql into the extension sharedir.
COPY --from=builder /src/target/release/pgokf-pg${PG_MAJOR}/usr/ /usr/

# Auto-create the extension in the default database on first init. This runs
# only when the data directory is empty (standard postgres entrypoint rule).
COPY packaging/docker/initdb/10-create-extension.sql /docker-entrypoint-initdb.d/10-create-extension.sql
