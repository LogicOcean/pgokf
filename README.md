# pgokf

**A PostgreSQL extension that turns [Open Knowledge Format](https://openknowledge.dev) bundles into a queryable, transactional catalog** - full-text, semantic, and hybrid search, a link graph, multi-tenant isolation, version history, and an audit trail, all inside PostgreSQL.

[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)
[![PostgreSQL 15–19](https://img.shields.io/badge/PostgreSQL-15%E2%80%9319-336791.svg)](https://www.postgresql.org/)
[![Built with pgrx](https://img.shields.io/badge/built%20with-pgrx%200.19-000000.svg)](https://github.com/pgcentralfoundation/pgrx)
[![Status: pre-1.0](https://img.shields.io/badge/status-pre--1.0-orange.svg)](docs/api-stability.md)

An OKF **bundle** is just a directory of UTF-8 Markdown "concept" documents with YAML frontmatter - runbooks, wikis, service catalogs, datasets. The bundle on disk stays the portable source of truth; `pgokf` materializes it into a PostgreSQL projection optimized for search and graph queries, and keeps them in sync incrementally.

- **Extension & schema:** `pgokf` · **OKF conformance:** v0.2 · **PostgreSQL:** 15–19
- **Built with:** Rust (edition 2024) + [pgrx](https://github.com/pgcentralfoundation/pgrx) 0.19 · **Safety:** `#![forbid(unsafe_code)]`, clippy-pedantic, `cargo deny`

---

## Highlights

- 🔎 **Search, four ways.** Native PostgreSQL full-text ranking out of the box; optional **BM25** (via ParadeDB `pg_search`) and **semantic** + **hybrid RRF** search (via `pgvector`) behind the same seam - none of them a hard dependency. Structured filters (`type`/`tags`/`status`/`trust_tier`), keyset pagination, faceted counts, and `find_similar` more-like-this.
- 🕸️ **Link graph.** Markdown cross-links and OKF *Attested Computation* references become resolved edges; `concept_neighbors` walks them (bounded, cycle-safe, BFS).
- 🏢 **Multi-tenant.** Opt-in row-level-security isolation keyed on a session tenant, backward-compatible (unset = see-all) - read *and* write confined.
- 🕰️ **Version history.** Opt-in point-in-time trail: `concept_history` and `concept_as_of('… last Tuesday')`.
- 🧾 **Audit & lifecycle.** A durable sync log + per-sync change manifest, an exfiltration/access log, reversible **retire**/**purge**, and cross-bundle **dedup**.
- 📥 **Two ingestion paths.** From a **filesystem** path, or **mountless** - bytes streamed from an S3-compatible object store, with the extension performing zero network I/O.
- 🧰 **Companion tools.** Object-store ingestion, a reference **embedder**, and an **MCP server** that exposes the catalog to AI agents.
- 📦 **Operable.** `catalog_stats()` / `health()` / `search_index_status()`, Parquet + source-file exports, `pg_cron` scheduled refresh, PGXN / `.deb` / `.rpm` / Docker packaging.

See the exact, versioned surface - every function, table, type, GUC, and role - in **[docs/sql-api.md](docs/sql-api.md)** and **[docs/api-stability.md](docs/api-stability.md)**.

## Quick start

Registration runs in the **PostgreSQL server process**, so the bundle path must be absolute and server-reachable, and it requires membership in `pgokf_writer`. A sample bundle ships in [`examples/sample-bundle/`](examples/sample-bundle).

```sql
CREATE EXTENSION pgokf;                       -- schema, tables, roles, functions
GRANT pgokf_writer TO myuser;                 -- reader < writer < admin

-- ingest a bundle (writer)
SELECT * FROM pgokf.register_bundle('/abs/path/to/examples/sample-bundle');

-- ranked full-text search, optionally filtered
SELECT concept_id, title, rank
FROM pgokf.concept_search('postgres failover', concept_type => 'runbook');

-- walk the resolved link graph
SELECT * FROM pgokf.concept_neighbors('runbooks/database-failover', 2);

-- browse the projection
SELECT id, title, type, tags FROM pgokf.concepts ORDER BY id;
```

**Semantic / hybrid search** (needs `pgvector`): supply embeddings with `set_concept_embedding` (or the `pgokf-embed` companion), build the index with `rebuild_embedding_index()`, then:

```sql
SELECT * FROM pgokf.concept_search_semantic( $query_vector );          -- nearest by cosine
SELECT * FROM pgokf.concept_search_hybrid('failover', $query_vector);  -- RRF fusion of lexical + vector
```

## Optional capabilities

Everything works with stock PostgreSQL; these unlock more when installed, and degrade cleanly when absent:

| Extension | Unlocks | Absent behavior |
| --------- | ------- | --------------- |
| `pgvector` | `concept_search_semantic`, `concept_search_hybrid`, embeddings | semantic errors clearly; hybrid falls back to lexical |
| `pg_search` (ParadeDB) | BM25 ranking (`search_backend = bm25`) | falls back to native FTS with a warning |
| `pg_cron` | `schedule_refresh` / `unschedule_refresh` | scheduling raises a clear "install pg_cron" error |

## Companion tools

Standalone binaries (in [`crates/`](crates)) that pair with the extension - credentials live in the companion, never in PostgreSQL, and each can connect over TLS:

| Tool | What it does |
| ---- | ------------ |
| [`pgokf-ingest`](crates/pgokf-ingest) | Mountless ingestion: reads an S3/MinIO/SeaweedFS bucket and streams it into the catalog. `--watch` re-syncs on change. |
| [`pgokf-embed`](crates/pgokf-embed) | Reference embedder: computes vectors via any OpenAI-compatible `/v1/embeddings` endpoint and stores them. |
| [`pgokf-mcp`](crates/pgokf-mcp) | A Model Context Protocol server exposing `concept_search` / `find_similar` / `concept_neighbors` as agent tools. |

## Documentation

Full docs are published at **<https://logicocean.github.io/pgokf/>**. Key entry points:

| Document | Covers |
| -------- | ------ |
| [getting-started](docs/getting-started.md) | Install, create the extension, grant a role, first queries |
| [sql-api](docs/sql-api.md) | Reference for every `pgokf.*` function, table, type, and GUC |
| [architecture](docs/architecture.md) | Parser, sync engine, projection seams, search backends |
| [search-guide](docs/search-guide.md) | Ranking, filters, pagination, BM25, semantic + hybrid |
| [multi-tenancy](docs/multi-tenancy.md) | Tenant isolation model, RLS, and its trust boundaries |
| [version-history](docs/version-history.md) | Opt-in temporal history and point-in-time queries |
| [deployment-topologies](docs/deployment-topologies.md) | Storage tiers, bucket-mount, mountless ingestion, Parquet |
| [operations](docs/operations.md) · [configuration](docs/configuration.md) | Day-2 ops, monitoring, upgrades; GUCs and policy keys |
| [security](docs/security.md) | Roles, `SECURITY DEFINER` model, path containment, least privilege |
| [okf-authoring](docs/okf-authoring.md) | Authoring OKF v0.2 bundles (frontmatter, actors, reserved files) |
| [api-stability](docs/api-stability.md) | The public API contract, SemVer policy, deprecation |

Runnable SQL is in [`examples/queries/`](examples/queries); reusable OKF templates in [`templates/`](templates); authoring/catalog skills in [`skills/`](skills).

## Building from source

```bash
cargo install cargo-pgrx --version 0.19.2 --locked
cargo pgrx init --pg18 $(which pg_config)                 # or your target major
cargo pgrx install --pg-config $(which pg_config) --features pg18
```

Select the major via the crate feature (`pg15`…`pg19`; default `pg18`). Run the gate:

```bash
cargo test -p pgokf --no-default-features --features pg18                     # unit + api-stability
RUST_TEST_THREADS=1 cargo pgrx test pg18 --no-default-features --features pg18 # in-database
```

> Run the in-database suite **single-threaded** - see [CONTRIBUTING.md](CONTRIBUTING.md) for why.

## Project status

Pre-1.0 (`0.1.x`). The enumerated SQL surface is treated as stable and every change ships an upgrade script verified `upgrade == fresh`, but per SemVer a `0.MINOR` bump may still carry a breaking change (called out in [CHANGELOG.md](CHANGELOG.md)). Reaching `1.0.0` is a deliberate decision, not an automatic bump.

## License

pgokf is **dual-licensed**: **AGPL-3.0-only** for all crates ([`LICENSE`](LICENSE); every source file carries an SPDX header), plus a **commercial license** for use the AGPL does not permit - embedding in a proprietary product, offering it as a managed service without releasing source, or an organizational no-AGPL policy. See [`LICENSING.md`](LICENSING.md) for the model and [`COMM-LICENSE.md`](COMM-LICENSE.md) for the commercial terms.

## Security & contributing

Report vulnerabilities privately - see [`SECURITY.md`](SECURITY.md). Contributions are welcome under a CLA (required by the dual-license model) - see [`CONTRIBUTING.md`](CONTRIBUTING.md).
