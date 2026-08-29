# Contributing to pgokf

Thanks for your interest in improving pgokf.

## Contributor License Agreement (required)

pgokf is **dual-licensed** (AGPL-3.0 + a commercial license - see
[`LICENSING.md`](LICENSING.md)). So that the project can keep offering the
commercial option, **every contribution must be covered by a signed
Contributor License Agreement (CLA)** granting LogicOcean a perpetual,
irrevocable license to the contribution, including the right to sublicense it
and to relicense it under any terms (including the commercial license),
together with a patent grant. **Until the CLA signing process is live,
external pull requests cannot be merged.** A pull request opened before then
will be held, not merged, until its author has signed; opening a pull request
does not by itself create a CLA. The CLA signing link will be added here when
the process is live.

If you cannot agree to the CLA, please open an issue describing the bug or idea
instead of a pull request - we can often implement it independently.

## Development setup

- Rust 1.96 (see `rust-toolchain.toml`), `cargo-pgrx` 0.19.2.
- Install a PostgreSQL 15–19 dev environment: `cargo pgrx init --pgNN=…`.
- Optional runtime extensions the seams probe for: `pgvector` (semantic search),
  ParadeDB `pg_search` (BM25), `pg_cron` (scheduled refresh) - all optional.

## Before you open a PR

Run the full local gate (what CI runs):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --no-default-features --features pg18 -- -D warnings
cargo deny check
cargo test -p pgokf --no-default-features --features pg18        # unit + api_stability
RUST_TEST_THREADS=1 cargo pgrx test pg18 --no-default-features --features pg18   # in-DB
```

> **Run the in-database suite single-threaded** (`RUST_TEST_THREADS=1`): pgrx
> wraps each `#[pg_test]` in one long transaction, so tests that touch the
> singleton config row can deadlock under the parallel harness. This is a
> test-harness artifact, not a production concern.

## Conventions

- Every new public SQL object needs a `COMMENT ON` and an entry in
  `crates/extension/tests/api_stability.rs` (the surface is locked).
- Any change to stored data ships an upgrade script
  `crates/extension/sql/pgokf--<from>--<to>.sql`, verified `upgrade == fresh`.
- New source files carry an `SPDX-License-Identifier: AGPL-3.0-only` header.
- See [`docs/api-stability.md`](docs/api-stability.md) for the versioning and
  compatibility rules.
