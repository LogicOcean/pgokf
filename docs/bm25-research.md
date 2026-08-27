# BM25 search backends for `pgokf`

Status: decision record. Last refreshed for PostgreSQL 15–19 and pgrx 0.19.2.
Grounded against the implemented `pgokf.concept_search`
(`crates/extension/src/catalog/search.rs`, HEAD `228e8e1`).

## Decision

**Native PostgreSQL full-text search (FTS) is the required, primary and only
shipped backend. BM25 is deferred to an *optional*, admin-enabled adapter that
sits behind a stable Strategy seam and is not implemented in this wave.** The
catalog already stores a weighted `tsvector` (`pgokf.concepts.body_tsv`),
indexes it with GIN, matches with `websearch_to_tsquery`, and ranks with
`ts_rank_cd`. This keeps `pgokf` installable on every supported server
(PostgreSQL 15–19, pgrx 0.19.2), including managed and locked-down environments
that forbid third-party `shared_preload_libraries`, while leaving a clean path
to corpus-aware BM25 retrieval for large deployments.

Do not vendor or revive the historical `pg_bm25` crate. It is an old ParadeDB
product name; the maintained successor is `pg_search`.[11][8]

## What `concept_search` does today

This section describes the *implemented* behavior, not an aspiration. Every
claim below is verifiable in the cited source.

**Signature and execution characteristics** — `search.rs:161-166`,
`search.rs:176-185`:

- `pgokf.concept_search(query text, bundle_id bigint DEFAULT NULL,
  limit_count int DEFAULT 20) RETURNS SETOF pgokf.concept_search_result`.
- Declared `STABLE` and `PARALLEL SAFE` (`#[pg_extern(stable, parallel_safe,
  …)]`), so the planner may run it in parallel workers and cache its result
  within a statement.
- Runs with **invoker rights** (no `SECURITY DEFINER`): it reads only tables
  `pgokf_reader` already holds `SELECT` on, so row visibility follows ordinary
  PostgreSQL grants (`search.rs:12-19`).
- `EXECUTE` is revoked from `PUBLIC` and granted only to `pgokf_reader`
  (`search.rs:176-181`), and `crate::security::authorize_current_user(
  Operation::Search, …)` re-checks the role policy as defense in depth
  (`search.rs:99`).

**Query and ranking** — `search.rs:71-88`:

- The user query is parsed with
  `websearch_to_tsquery(<configured text-search config>, $1)` (the
  `default_text_search_config` setting, default `pg_catalog.english`) — the
  web-style parser that tolerates unbalanced quotes and bare operators, so
  untrusted input never raises a syntax error.
- Matching predicate is `c.body_tsv @@ q.query`, evaluated against the GIN
  index `concepts_body_tsv_gin` (`schema.rs:72`).
- Relevance is `ts_rank_cd(c.body_tsv, q.query)` — cover-density ranking, which
  rewards proximity of matching lexemes.
- Ordering is `ORDER BY ts_rank_cd(...) DESC, c.id ASC`. The concept `id` is a
  deterministic tiebreaker, so equal-rank pages are stable across runs.
- Only enabled bundles participate (`JOIN pgokf.bundles b ON b.id =
  c.bundle_id AND b.enabled`); an optional `bundle_id` narrows to one bundle.
- `limit_count` is validated to the inclusive range `1..=500` (default `20`);
  out-of-range values raise SQLSTATE `22023` (`search.rs:30-51`).

**The weighted vector it ranks over** — built during sync, not by a generated
column or trigger (`sync.rs:296-321`):

| Weight | Source fields | SQL |
|:---:|---|---|
| `A` | `title` | `setweight(to_tsvector('pg_catalog.english', title), 'A')` |
| `B` | `tags`, `type`, `description` | `setweight(to_tsvector('pg_catalog.english', concat_ws(' ', array_to_string(tags,' '), type, description)), 'B')` |
| `D` | `body_text` | `setweight(to_tsvector('pg_catalog.english', body_text), 'D')` |

`body_tsv` is recomputed inside the `register_bundle`/`refresh_bundle` upsert
whenever a concept file changes, so it stays consistent without a trigger and
without a second write path.

**Snippets** — each hit carries a `ts_headline('pg_catalog.english',
concat_ws(' ', c.title, c.description, c.body_text), q.query)` excerpt
(`search.rs:78-81`), surfaced as the `headline` column of
`concept_search_result`.

Net effect: `concept_search` is a self-contained, corpus-*independent* ranker.
`ts_rank_cd` weighs matched-lexeme frequency and proximity, with A/B/D field
weights, but has no notion of global term rarity (IDF) or document-length
normalization. That single property is the whole reason to evaluate BM25.

## What was evaluated

### PostgreSQL native `ts_rank_cd` (the shipped baseline)

`ts_rank` ranks on matching-lexeme frequency; `ts_rank_cd` additionally scores
cover density (proximity) and requires positional, unstripped lexemes.[3][4]
Both accept A–D field weights and a normalization bitmask (document length,
unique-word count, extent distance, or `rank/(rank+1)` scaling).[3][4] These
primitives are unchanged across PostgreSQL 15–19; the linked PG15 and PG18
manuals are identical on this point.[3][4]

The defining limitation is that PostgreSQL's ranking functions "do not use any
global information," so a term that is common across the corpus is not
penalized and a term that is rare is not boosted.[3][4] BM25 supplies exactly
that missing signal: inverse document frequency, `k1` term-frequency
saturation, and `b` document-length normalization.

| Property | Native `ts_rank_cd` (shipped) | BM25 (`pg_search` et al.) |
|---|---|---|
| Required dependency | PostgreSQL only | Extra native extension + server config |
| PG15–19 coverage | Full (15,16,17,18,19) | Best case PG15–18; none covers PG19 yet |
| Relevance inputs | weighted term frequency + match proximity | corpus rarity (IDF), saturated TF, doc length |
| Index | GIN over `tsvector`; rank evaluated per-row | custom inverted/columnar index with top-k execution |
| Small OKF bundles | Sufficient and simple | Usually unnecessary operational weight |
| Large heterogeneous corpora | Can over-reward frequent terms; rank compute grows | Usually stronger relevance/top-k; must be benchmarked |
| Portability | High (managed PG included) | Low, especially managed PG and PG19 |

`ts_rank_cd` is not "BM25-lite": proximity can make it better for phrase-like
queries, while IDF and length normalization can make BM25 better across
documents of very different lengths. Which wins on OKF data is empirical — see
the benchmark plan — so no universal winner should be assumed.

### ParadeDB: `pg_bm25` → `pg_search`

The historical ParadeDB `pg_bm25` extension used Tantivy through pgrx, required
`shared_preload_libraries`, and exposed BM25 search, highlighting, filtering,
fuzzy and hybrid search.[11] It was renamed `pg_search` by v0.8.0, whose Cargo
features targeted PostgreSQL 12–16.[9]

The maintained extension is now `pg_search`. Its development README states
support "starting at PostgreSQL 15" and targets PostgreSQL 15–18.[8] Current
Community packages require `shared_preload_libraries = 'pg_search'` and, since
0.25.0, a `pgvector` (`vector`) dependency available before `CREATE
EXTENSION`.[8][12] Community ParadeDB is AGPL-3.0.[1] It stores an inverted +
columnar index in LSM-tree segments, uses a custom scan for its operators, and
exposes scores via `pdb.score(key)` (higher = more relevant; add a primary-key
tiebreaker for determinism).[7][6] The index is transactionally colocated with
the source table, avoiding a separate search service.[1]

Consequences for `pgokf`: **does not cover PG19**, needs a server restart /
preload setting, ships native binaries, drags in `pgvector`, and is unlikely to
be available on arbitrary managed PostgreSQL. Its scoring, top-k, filtering,
highlighting, and tokenizer surface are attractive at large corpus sizes but
exceed Phase 1 needs.

### Timescale `pg_textsearch`

A PostgreSQL-native BM25 index with configurable `k1` (TF saturation, default
1.2) and `b` (length normalization, default 0.75), PostgreSQL text-search
configurations, expression/partial indexes, partitioned-table support, parallel
index builds, and Block-Max WAND top-k execution.[13] Current support is
**PostgreSQL 17–18 only** (v1.5.0-dev, "production ready"), so it cannot serve
PG15–16 or PG19.[13] Notably it is **PostgreSQL-OSS licensed** — the most
permissive option surveyed — and needs no `pgvector`. If its version matrix
widens to PG15–19 it becomes the most attractive adapter target; today its
narrow matrix disqualifies it as the Phase 1 backend.

### VectorChord-BM25

Implements BM25 vectors plus a BM25 index, and is designed to pair with a
separate `pg_tokenizer` extension for customized tokenization (`CREATE
EXTENSION pg_tokenizer CASCADE`).[14] Current builds target **PostgreSQL 17–18**
and it is dual-licensed AGPLv3 / Elastic License v2.[14] It suits hybrid /
vector-oriented stacks, but the second representation plus a tokenizer
lifecycle is disproportionate for a catalog baseline, and it too omits PG15–16
and PG19.

No alternative currently offers a cleaner PG15–19 story than native FTS.

## Recommended `pgokf` design

### Now: native FTS is the required backend

1. **Native search data is always present.** `body_tsv` is a weighted
   `tsvector` (title A; tags/type/description B; body D) maintained by the sync
   upsert and indexed by GIN — already implemented; keep it as the invariant.
2. **`pgokf.concept_search(query, bundle_id, limit_count)` is the stable public
   contract.** Its columns, authorization, deterministic tiebreak, score
   direction, and `1..=500` limit are the interface any future backend must
   preserve.
3. **No transitive third-party dependency.** `CREATE EXTENSION pgokf` must
   never require ParadeDB, Timescale, or VectorChord.

### Later (out of scope for this wave): an optional BM25 adapter as a Strategy seam

This is a *design sketch for a future task*, not code to write here. It scopes
how a BM25 backend could be added without destabilizing the core.

- **Strategy interface.** Introduce a `SearchBackend` trait with a single
  method, roughly:

  ```rust
  trait SearchBackend {
      fn search(
          &self,
          query: &str,
          bundle_id: Option<i64>,
          limit: i64,
      ) -> Result<Vec<SearchHit>, CatalogError>;
  }
  ```

  `concept_search_impl` keeps doing validation + authorization, then delegates
  to the selected backend. The existing native implementation becomes
  `NativeFtsBackend` (a straight extraction of today's `SEARCH_QUERY`); a
  `Bm25Backend` is added later. `SearchHit` and `concept_search_result` stay
  the shared output type, so the SQL contract is untouched.
- **Selection is data, not a rebuild.** A GUC — e.g. `pgokf.search_backend`
  with values `native` (default) | `bm25` — chooses the strategy at call time
  via a small factory. Because the interface returns identical rows, callers
  see no schema change.
- **Feature-gated compilation.** Gate the BM25 backend behind a Cargo feature
  (`--features bm25`) so the default build has zero extra dependencies and the
  core PG15–19 matrix is unaffected. The adapter, its vendor-specific index
  DDL, and any `pdb.score`/BM25 operators live entirely inside that module.
- **Fail closed to native, never to broken search.** If `search_backend =
  bm25` but the extension is absent, version-incompatible, or the feature was
  not compiled in, either reject the GUC at set time with an actionable error
  or emit a warning and fall back to `native` — never return a silently empty
  or malformed result set.
- **Do not compare raw scores across algorithms.** Backend switches change
  score *scale*; only ordering (with the ID tiebreak) is contract.

## Compatibility summary (upstream-declared targets)

| Backend / version line | PG15 | PG16 | PG17 | PG18 | PG19 | Notes |
|---|:---:|:---:|:---:|:---:|:---:|---|
| Native `tsvector` + `ts_rank_cd` (shipped) | ✓ | ✓ | ✓ | ✓ | ✓ | Required baseline; primitives unchanged 15–19[3][4] |
| Historical `pg_bm25` 0.5.x | src | src | — | — | — | Obsolete product name; do not adopt[11] |
| ParadeDB `pg_search` 0.8.x | src | src | — | — | — | Historical; not a recommended pin[9] |
| Current ParadeDB `pg_search` | ✓ | ✓ | ✓ | ✓ | — | Targets PG15–18; needs preload + pgvector; AGPL-3.0[8][12][1] |
| Timescale `pg_textsearch` | — | — | ✓ | ✓ | — | v1.5.0-dev; PostgreSQL-OSS license; k1/b + Block-Max WAND[13] |
| VectorChord-BM25 | — | — | ✓ | ✓ | — | Needs `pg_tokenizer`; dual AGPLv3/ELv2[14] |

"Compatibility" is the upstream build/support target, not a `pgokf`
certification. Any optional adapter must be CI-tested against exact extension
releases and OS packages independently of the core PG15–19 matrix.

## Benchmark plan — 10K-concept comparison

This is the runnable methodology for the separate benchmark task. **No numbers
below are measured.** Any figure that appears is a projection labeled as such;
real runs must replace them.

### Environment

- One PostgreSQL major per run (15,16,17,18,19), pgrx 0.19.2, `pgokf` at the
  commit under test. For BM25 arms, pin the exact extension release and its
  preload/pgvector requirements; BM25 arms are skipped on majors the extension
  does not support (e.g. PG19 for all three; PG15–16 for `pg_textsearch` and
  VectorChord).
- Fixed hardware; `shared_buffers`, `work_mem`, `max_parallel_workers_per_gather`
  recorded and held constant across arms. Warm and cold-cache passes
  (`pg_prewarm` vs. restart + drop OS cache) reported separately.

### Datasets

Generate synthetic OKF bundles at three scales so trends, not a single point,
are visible:

| Scale | Concepts | Purpose |
|---|---:|---|
| S | 1,000 | fits in cache; isolates per-query CPU |
| **M** | **10,000** | the headline comparison |
| L | 100,000 | index-size / spill behavior |

Each concept carries a title, 3–8 tags, a type, a 1–3 sentence description, and
a body of 50–500 words drawn from a mixed vocabulary (common + long-tail rare
terms) so IDF differences are exercised. Links between concepts average ~4 out-
edges/node to give the graph traversal something real to walk. Fix the RNG
seed and commit the generator so runs are reproducible.

### Workloads (each timed independently)

1. **Bulk load** — `register_bundle(path)` for the whole bundle from cold; also
   an incremental `refresh_bundle` after touching 5% of files. Captures ingest
   throughput and `body_tsv` (re)compute cost.
2. **Filtered scan** — `list_bundles()` / `bundle_info()` and a
   `bundle_id`-scoped `concept_search` to measure selective-predicate latency.
3. **FTS relevance** — a fixed query set run through `concept_search`, covering:
   title-heavy, body-heavy, single rare term, repeated common term, multi-word
   phrase, and mixed. For each query capture latency **and** ranking quality
   against a judged qrels file (nDCG@10 and MRR@10). Score *ordering* is
   compared across backends with the `id` tiebreak; raw scores are not compared.
4. **Graph traversal** — `concept_neighbors(concept_id, max_hops, bundle_id)` at
   `max_hops` 1/2/3 (bounded by `pgokf.max_graph_hops`) from a fixed seed set,
   measuring recursive-CTE latency vs. hop depth and fan-out.

### Metrics

- **Latency**: p50 / p95 / p99 per workload, ≥1,000 timed iterations after
  warmup, driven by `pgbench -f` custom scripts or `EXPLAIN (ANALYZE, BUFFERS)`;
  report shared/local buffer hits.
- **Relevance**: nDCG@10, MRR@10 from judged qrels (FTS workload only).
- **Memory**: peak backend RSS during load and query; `work_mem` spills.
- **Disk**: `pg_total_relation_size` of `concepts` + each search index
  (`concepts_body_tsv_gin` vs. the BM25 index), and WAL volume during load.
- **Build**: index build wall-time and whether parallel builds were used.

### Methodology and reporting

- Three trials per (major × arm × workload); report median and IQR, discard the
  first (cold JIT/plan) trial.
- Keep query text, seeds, and qrels in-repo next to the generator.
- Publish a single results table per major with the native arm as the baseline
  column and BM25 arms as deltas. A BM25 arm is promoted only if it shows a
  material, reproducible relevance or top-k-latency win at scale M/L that
  justifies its operational and licensing cost.
- **Illustrative shape only, not a result (projection):** native GIN+`ts_rank_cd`
  is expected to lead on load time and index size, while BM25 is expected to
  lead on rare-term relevance at scale L. These are hypotheses to test, not
  findings; delete this line once measured data exists.

## Validation plan (adapter, when built)

- Establish native-FTS relevance snapshots against all fixtures first; treat
  them as the regression baseline.
- Add an optional BM25 CI job (not required for ordinary PRs) that provisions a
  supported extension release on a supported major and runs the same query
  contract; the job self-skips on unsupported majors.
- Compare backend result *ordering* with the deterministic `id` tiebreak; do
  not compare raw scores across algorithms.
- Test insert/update/delete synchronization and `VACUUM`; ParadeDB documents
  that dead rows can affect scores until vacuum removes them.[6]
- Review AGPL / ELv2 redistribution implications before publishing any image or
  package that bundles a Community BM25 extension.[1][14]

## Sources

[1] https://github.com/paradedb/paradedb — ParadeDB repository (AGPL-3.0 Community)
[3] https://www.postgresql.org/docs/15/textsearch-controls.html — PostgreSQL 15: Controlling Text Search
[4] https://www.postgresql.org/docs/18/textsearch-controls.html — PostgreSQL 18: Controlling Text Search
[6] https://docs.paradedb.com/documentation/sorting/score — ParadeDB relevance scoring
[7] https://docs.paradedb.com/welcome/architecture — ParadeDB architecture
[8] https://github.com/paradedb/paradedb/blob/dev/pg_search/README.md — pg_search development README (PG15–18, preload + pgvector)
[9] https://github.com/paradedb/paradedb/blob/v0.8.0/pg_search/Cargo.toml — ParadeDB v0.8.0 pg_search Cargo features (PG12–16)
[11] https://github.com/paradedb/paradedb/blob/v0.5.11/pg_bm25/README.md — Historical pg_bm25 README (v0.5.11)
[12] https://docs.paradedb.com/deploy/self-hosted/extension — ParadeDB self-hosted extension installation
[13] https://github.com/timescale/pg_textsearch — Timescale pg_textsearch (PG17–18, PostgreSQL-OSS, k1/b, Block-Max WAND)
[14] https://github.com/tensorchord/VectorChord-bm25 — VectorChord-bm25 (PG17–18, pg_tokenizer, AGPLv3/ELv2)
