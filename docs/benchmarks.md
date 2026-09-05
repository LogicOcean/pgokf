# OKF catalog format benchmark: YAML vs PostgreSQL vs Parquet

This document reports **real, measured** numbers for three ways of holding an
Open Knowledge Format (OKF) catalog on one host:

- **YAML** - the raw Markdown-with-frontmatter bundle on disk, parsed on demand
  through `okf_parser::parse_concept` (the same entry point the extension uses).
- **PostgreSQL** - the `pgokf` extension catalog: `concepts`, `links`,
  `concept_metadata`, and `concept_provenance` with their btree/GIN indexes and
  the weighted `body_tsv` full-text column. The search leg measures the
  **default native FTS backend**; the optional BM25 (`pg_textsearch` / `pg_search`) and pgvector
  semantic/hybrid backends are not exercised here.
- **Parquet** - the columnar snapshot produced by `pgokf.export_parquet`
  (zstd-compressed, one file per exported projection table: `concepts`,
  `links`, `concept_metadata`, `concept_provenance`).

Every number below was produced by `scripts/bench/run_bench.sh` on the host
named in the results. Nothing here is estimated. Where a leg could not be
measured it is marked explicitly.

## Host and environment

Captured by the harness at run time:

| Fact | Value |
| --- | --- |
| Repo HEAD | `30777d2` |
| CPU | 72 logical cores (`nproc`) |
| Memory | 251 GiB total (`free -h`) |
| PostgreSQL | `PostgreSQL 18.6 (Ubuntu 18.6-1.pgdg24.04+2)` (`pg_config --version`) |
| Rust | `rustc 1.96.0 (ac68faa20 2026-05-25)` |
| pgrx / cargo-pgrx | 0.19.2 (PG18 feature) |
| Parquet reader | `parquet`/`arrow` 59.2.0 (the extension's own versions) |
| Dataset | 12,000 generated OKF concepts (seed 1337) |

Reader availability: `pyarrow` and `duckdb` are **not installed** on the
benchmark host (neither the Python modules nor a `duckdb` CLI). The Parquet read
leg therefore uses a tiny throwaway Rust reader built against the same
`parquet`/`arrow` 59.2.0 crates the extension depends on - not an estimate, a
real decode of the exported file. See "Parquet read leg" below.

## Dataset

`scripts/bench/generate_corpus.py` deterministically generates a bundle of
12,000 concepts (fixed seed → byte-identical output on every run):

- YAML frontmatter with `type`, `title`, `description`, and a `tags` list;
- a realistic multi-paragraph Markdown body (a few hundred tokens);
- three resolvable internal links per concept (root-relative, guaranteed to
  point at concepts that exist), plus one external link, plus an occasional
  deliberately unresolved internal link;
- OKF v0.2 provenance/trust/lifecycle frontmatter (`generated`, `verified`,
  `status`, `stale_after`, `sources`) on 30% of files.

The generator writes a `manifest.json` carrying the exact expected row counts
for the type and tag filters, a traversal seed concept, and the FTS query, so
the orchestrator drives representative queries without hard-coding generation
details. Observed projection after registration:

| Table | Rows |
| --- | --- |
| `concepts` | 12,000 |
| `links` | 48,700 |
| `concept_metadata` | 16,800 |
| `concept_provenance` | 3,600 |

The query row counts validate the corpus end to end: the `type='Runbook'`
filter returned exactly **1,500** rows (manifest expected 1,500) and the
`tags @> {postgres}` filter returned exactly **4,000** rows (expected 4,000).

## How to reproduce

```bash
# From the repo root. Installs the extension into the local PG18, spins up a
# throwaway cluster under ${TMPDIR:-/tmp}/pgokf-bench, runs all three legs,
# prints the results table, and cleans up the cluster + corpus on exit.
scripts/bench/run_bench.sh
```

Environment overrides (defaults in parentheses): `OKF_BENCH_COUNT` (12000),
`OKF_BENCH_SEED` (1337), `OKF_BENCH_PORT` (54329), `PGOKF_PG_CONFIG`
(`/usr/lib/postgresql/18/bin/pg_config`), `OKF_BENCH_QUERY_REPEATS` (6).

The individual steps the script runs, for reference:

```bash
# (a) YAML leg
python3 scripts/bench/generate_corpus.py --count 12000 --out /tmp/corpus --seed 1337
cargo run --release --example parse_all -p okf-parser -- /tmp/corpus

# (b) PostgreSQL leg
cd crates/extension
cargo pgrx install --no-default-features --features pg18 \
    --pg-config /usr/lib/postgresql/18/bin/pg_config --sudo --release
initdb -D "$PGDATA" --locale=C.UTF-8 -A trust
pg_ctl -D "$PGDATA" -o "-p 54329 -k /tmp/pgokf-bench/okf-bs -c listen_addresses=''" -w start
psql -h /tmp/pgokf-bench/okf-bs -p 54329 -d okfbench -c "CREATE EXTENSION pgokf"
psql ... -c "SELECT * FROM pgokf.register_bundle('/tmp/corpus')"        -- bulk load
psql ... -c "SELECT count(*) FROM pgokf.concepts WHERE type = 'Runbook'"           -- btree
psql ... -c "SELECT count(*) FROM pgokf.concepts WHERE tags @> ARRAY['postgres']"  -- GIN
psql ... -c "SELECT count(*) FROM pgokf.concept_search('replication failover', NULL, 500)"
psql ... -c "SELECT count(*) FROM pgokf.concept_neighbors('services/0000/c000000', 3)"

# (c) Parquet leg
psql ... -c "SELECT * FROM pgokf.export_parquet(1, '/tmp/parquet')"
# concepts.parquet is then decoded + filtered by a tiny parquet-crate reader.
```

Notes on methodology:
- The extension is installed with `--release` (cargo-pgrx defaults to debug);
  a debug `.so` would make the in-server parse path and queries unrepresentative.
- After the bulk load the harness runs `ANALYZE` on the `pgokf` tables, as a DBA
  would, so the planner picks representative plans.
- Each query is timed server-side via `psql \timing`, run 6 times; the first run
  is discarded as warm-up and the **median of the remaining 5** is reported.
- The scratch cluster's socket lives in a deliberately short scratch base
  (`${TMPDIR:-/tmp}/pgokf-bench`) to stay within the 107-byte UNIX socket path
  limit; point `TMPDIR` somewhere equally short if you override it.

## Results (real, measured)

Single coherent run on the host above:

```
========================================================================
 OKF catalog format benchmark - REAL measured results
========================================================================
 host        : 72 cores, 251Gi RAM
 postgres    : PostgreSQL 18.6 (Ubuntu 18.6-1.pgdg24.04+2)
 rustc       : rustc 1.96.0 (ac68faa20 2026-05-25)
 dataset     : 12000 concepts (12000 files, seed 1337)
 rows        : concepts=12000 links=48700 provenance=3600 metadata=16800
------------------------------------------------------------------------
 Metric                             | Value
------------------------------------------------------------------------
 Disk: YAML corpus dir              | 49M (30152279 bytes)
 Disk: PG catalog (tbl+idx)         | 68MB (71532544 bytes)
 Disk: Parquet (4 files)            | 4.8M (4937374 bytes)
------------------------------------------------------------------------
 YAML: parse-all (okf_parser)       | 611.1 ms  (19636.3 files/s, RSS 4.8 MiB)
 PG: register_bundle (bulk load)    | 20848.5 ms
 PG: filtered scan by type          | 2.770 ms  (1500 rows)
 PG: filtered scan by tag GIN       | 14.586 ms  (4000 rows)
 PG: FTS concept_search             | 343.313 ms  (500 rows)
 PG: graph traversal (3 hops)       | 5.348 ms  (38 rows)
 Parquet: export_parquet            | 439.245 ms
 Parquet: filtered read (type)      | 55.90 ms  (1500/12000 rows match/total)
========================================================================
```

### Disk footprint

| Format | On disk | vs YAML | Notes |
| --- | ---: | ---: | --- |
| YAML corpus dir | 30,152,279 B (49 MB) | 1.00× | raw editable source (~2.5 KB/file) |
| PG catalog (tables + indexes) | 71,532,544 B (68 MB) | 2.37× | includes `body_tsv` + 2 GIN + btree indexes |
| Parquet (4 files) | 4,937,374 B (4.8 MB) | 0.16× | zstd columnar; **no** search index |

PostgreSQL catalog breakdown (from `pg_relation_size` / `pg_indexes_size` /
`pg_total_relation_size`):

| Table | Heap | Indexes | Total |
| --- | ---: | ---: | ---: |
| `concepts` | 18 MB | 12 MB | 53 MB |
| `links` | 6424 kB | 4040 kB | 10 MB |
| `concept_metadata` | 2000 kB | 1576 kB | 3608 kB |
| `concept_provenance` | 1440 kB | 232 kB | 1704 kB |
| `bundles` | 8192 B | 32 kB | 48 kB |

Parquet file breakdown:

| File | Bytes |
| --- | ---: |
| `concepts.parquet` | 4,276,872 |
| `links.parquet` | 575,701 |
| `concept_metadata.parquet` | 47,910 |
| `concept_provenance.parquet` | 36,891 |

The PG catalog is the largest because it carries what makes it queryable: the
weighted `tsvector` search column, the GIN index over it, the GIN index over
`tags`, and btree indexes on `type`/`path`. Parquet drops all of that - it is a
columnar *data* snapshot, not a query engine - which is why zstd shrinks it to
~16% of the raw YAML and ~7% of the PG catalog.

### Operations

| Operation | YAML | PostgreSQL | Parquet |
| --- | ---: | ---: | ---: |
| Build / load the store | 611 ms (parse only) | 20,849 ms (parse + tsvector + indexes) | 439 ms (export from PG) |
| Filter by type (1,500/12,000) | ~611 ms† | **2.77 ms** (btree) | 55.9 ms (full decode + scan) |
| Filter by tag (4,000/12,000) | ~611 ms† | 14.6 ms (GIN) | - |
| Full-text search | not available‡ | 343 ms (rank + snippet, top 500) | not available‡ |
| Graph traversal (3 hops) | not available‡ | 5.35 ms (recursive CTE) | - |

† YAML has no index: answering *any* single filter means parsing the whole
bundle first, i.e. the ~611 ms parse-all cost, before you can filter at all.

‡ FTS ranking/snippets and graph traversal are not primitives of a flat YAML
tree or a Parquet file; they are what the PostgreSQL projection exists to add.

## Parquet read leg

No `pyarrow` and no `duckdb` (Python module or CLI) are installed on the
benchmark host. Rather than estimate, the harness builds a small throwaway Rust
reader against the same `parquet`/`arrow` 59.2.0 crates the extension already
uses, opens `concepts.parquet`, decodes **every** row group, and counts the rows
whose `type` column equals `Runbook`. This is a genuine end-to-end decode:

- rows scanned: 12,000; rows matched: 1,500 (matches the PG btree count);
- elapsed: **55.90 ms**; reader peak RSS: 17.5 MiB.

Because the export is not clustered by `type`, there is no row-group skipping -
this is a full-file decompress-and-scan, which is the honest cost of an ad-hoc
filter over a columnar snapshot with no secondary index.

## Honest conclusions: where each format wins

- **Parquet wins on disk and on cold analytical scans.** At 4.8 MB it is ~6×
  smaller than the raw YAML and ~15× smaller than the PG catalog, and a filtered
  full-file scan (55.9 ms) is ~11× faster than re-parsing the YAML bundle
  (611 ms) with no database process at all. It is the right format for archival
  snapshots, data interchange, and column-oriented analytics - but it has no
  indexes, no full-text search, and no graph traversal.

- **PostgreSQL wins decisively for repeated, selective, and relational
  queries.** Once the one-time ~20.8 s bulk load has built the indexes and the
  `tsvector`, an indexed filter answers in **2.8 ms** (type, btree) or 14.6 ms
  (tag, GIN), a 3-hop graph traversal in **5.3 ms**, and ranked full-text search
  with highlighted snippets in ~343 ms. None of these are possible over YAML or
  Parquet without reimplementing an index. The cost is disk (68 MB, 2.4× the
  source) and that up-front load. The FTS figure is heavier than the raw filters
  because `concept_search` also computes `ts_rank_cd` **and** a `ts_headline`
  snippet over title+description+body for the top 500 hits - snippet generation,
  not the index probe, dominates that number.

- **YAML wins as the source of truth.** It is human-editable, diffable,
  reviewable, and needs no engine - and `okf_parser` chews through all 12,000
  files in ~0.6 s at ~4.8 MiB peak RSS with zero errors. But it is write-optimized
  for humans, not read-optimized for queries: every question requires a full
  re-parse, and it offers no search or graph primitives. It is the input the
  other two formats are derived from, not a serving layer.

**Takeaway:** keep the OKF bundle in YAML as the authored source, project it into
the PostgreSQL catalog for live indexed search/graph/relational queries, and
export to Parquet for compact snapshots and downstream analytics. The three are
complementary, not competitors.

## Run-to-run variance (transparency)

Timings are wall-clock on a shared host and vary with page-cache warmth. The
YAML parse-all leg, for example, measured 1,255 ms (≈9,559 files/s, cold cache)
on a first invocation and 611 ms (≈19,636 files/s, warm cache) on the reported
run; both are real. The disk-footprint and row-count figures are deterministic
and identical across runs. For the tightest numbers, run
`scripts/bench/run_bench.sh` two or three times and take the warm run.
