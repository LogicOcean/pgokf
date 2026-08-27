# BM25 search backends for `pgokf`

## Decision

**Use PostgreSQL native full-text search as the required, primary backend and make BM25 an optional accelerator.** The baseline implementation should store a weighted `tsvector`, index it with GIN, and rank with `ts_rank_cd`. A later adapter may use ParadeDB `pg_search` when it is installed and explicitly enabled. This keeps `pgokf` installable on PostgreSQL 15–19, including environments that do not permit third-party shared-preload extensions, while leaving a path to higher-quality large-corpus retrieval.

Do not vendor or revive the historical `pg_bm25` crate. It is an old ParadeDB product name; the maintained successor is `pg_search`.

## What was evaluated

### ParadeDB: `pg_bm25` → `pg_search`

The historical ParadeDB `pg_bm25` extension used Tantivy through `pgrx`, required `shared_preload_libraries`, and exposed BM25 search, highlighting, filtering, fuzzy search, custom tokenizers, and hybrid search.[11] By ParadeDB v0.8.0 the component was named `pg_search`; that release's Cargo features included PostgreSQL 12–16, including 14–16.[9]

The maintained ParadeDB extension is now `pg_search`. Its current development branch states that supported PGDG releases begin at PostgreSQL 15 and currently targets PostgreSQL 15–18.[8] Current prebuilt Community packages are likewise documented for PostgreSQL 15+ and require both `shared_preload_libraries = 'pg_search'` and, since 0.25.0, `pgvector`.[12] Therefore current `pg_search` does **not** cover the full PG15–19 matrix; PG19 is not yet a supported target.

ParadeDB stores an inverted and columnar index in LSM-tree segments and invokes a custom scan for its operators.[7] It exposes BM25 scores through `pdb.score(key)`; higher scores are more relevant, and deterministic result sets should add a primary-key tiebreaker.[6] Its index remains transactionally colocated with the source table, which avoids a separate search service and synchronization pipeline.[1]

Operational and licensing considerations:

- Community ParadeDB is AGPL-3.0.[1]
- It adds a separately installed extension, a server restart/shared preload setting, native binaries, and (current releases) a `vector` dependency.[12]
- It is unlikely to be available on arbitrary managed PostgreSQL services.
- Its SQL/index surface is larger than `pgokf` needs for Phase 1, but its scoring, top-k execution, filtering, highlighting, and tokenization are attractive at larger corpus sizes.

### PostgreSQL native `ts_rank_cd`

PostgreSQL 14 and 17 both provide the same native ranking primitives. `ts_rank` ranks using matching lexeme frequency; `ts_rank_cd` additionally considers proximity (cover density) and requires positional, unstripped lexemes.[3][4] Both accept A–D field weights and a normalization bitmask for document length, unique-word count, extent distance, or cosmetic `rank/(rank+1)` scaling.[3][4]

The key difference from BM25 is corpus awareness. PostgreSQL's documentation explicitly notes that its ranking functions do not use global information.[3][4] BM25 uses corpus-level document frequency (IDF), term-frequency saturation, and document-length normalization. In practical terms:

| Property | Native `ts_rank_cd` | BM25 (`pg_search`) |
|---|---|---|
| Required dependency | PostgreSQL only | Additional native extension and server configuration |
| PG15–19 coverage | Yes | Current `pg_search`: PG15–18; no PG19 support yet |
| Relevance inputs | weighted term frequency and match proximity | corpus rarity, saturated term frequency, document length; field/query boosts in ParadeDB |
| Index | GIN/GiST over `tsvector` (ranking is evaluated separately) | custom ParadeDB inverted/columnar index with top-k execution |
| Small OKF bundles | Usually sufficient and simple | Usually unnecessary operational weight |
| Large heterogeneous corpora | Can over-reward repeated common terms; rank computation may become costly | Usually the stronger relevance/top-k choice; must be benchmarked on OKF data |
| Portability | High | Lower, especially on managed PostgreSQL and PG19 |

`ts_rank_cd` is not "BM25-lite": proximity can make it better for phrase-like queries, while BM25's IDF and saturation can make it better for relevance across documents of very different lengths. Which produces better OKF results is empirical, so evaluation should use judged queries rather than assuming one universal winner.

### Other extensions

- **Timescale `pg_textsearch`** provides a PostgreSQL-native BM25 index, configurable `k1`/`b`, PostgreSQL text-search configurations, expression/partial indexes, and Block-Max WAND top-k execution. Its current support is PostgreSQL 17–18 only, so it cannot be the Phase 1 backend for PG15–16 or PG19.[13]
- **VectorChord-BM25** implements BM25 vectors and a BM25 index and is commonly paired with a separate tokenizer extension. It is useful for hybrid/vector-oriented stacks, but introduces a second representation and tokenizer lifecycle that is disproportionate for the catalog baseline.[14]

Neither alternative currently offers a cleaner PG15–19 compatibility story than native FTS.

## Recommended `pgokf` design

1. **Always build native search data.** Store a generated or trigger-maintained weighted `tsvector` from title (A), tags/type/description (B), and body (C), indexed by GIN.
2. **Keep one stable API.** `pgokf.concept_search(query)` should remain the public contract. Backend selection is internal and must preserve columns, authorization behavior, deterministic tiebreaking, and sensible score direction.
3. **Make BM25 opt-in.** Detect `pg_search` at runtime and use it only when an administrator enables the backend. Never make `CREATE EXTENSION pgokf` transitively require ParadeDB.
4. **Fail closed to native, not silently to broken search.** If BM25 is configured but unavailable or version-incompatible, emit a clear diagnostic and either reject the setting or explicitly fall back with a warning.
5. **Do not expose ParadeDB operators in the core API.** Keep vendor-specific index DDL and `pdb.score` inside an adapter/migration layer.
6. **Benchmark before promotion.** Use the `large` fixture plus realistic 1k/10k/100k corpora, and record p50/p95 latency, index size, ingest/update cost, and judged relevance (MRR or nDCG@10). Include title-heavy, body-heavy, rare-term, repeated-term, phrase, CJK, and RTL queries.

## Compatibility summary (as researched)

| Backend/version line | PG15 | PG16 | PG17 | PG18 | PG19 | Notes |
|---|:---:|:---:|:---:|:---:|:---:|---|
| Native `tsvector` + `ts_rank_cd` | ✓ | ✓ | ✓ | ✓ | ✓ | Required baseline |
| Historical `pg_bm25` 0.5.x | source feature | source/default | — | — | — | Obsolete product name; do not adopt |
| ParadeDB `pg_search` 0.8.x | source feature | source/default | — | — | — | Historical, not a recommended pin |
| Current ParadeDB `pg_search` | ✓ | ✓ | ✓ | ✓ | — | Current branch targets PG15–18; packages documented for PG15+ |
| Current Timescale `pg_textsearch` | — | — | ✓ | ✓ | — | Current README supports PG17–18 |
| VectorChord-BM25 | not selected | not selected | not selected | not selected | not selected | Separate BM25-vector/tokenizer model |

Compatibility refers to upstream-declared build/support targets, not a `pgokf` certification. If an optional adapter is implemented, CI must test exact extension releases and OS packages independently of the core PG15–19 matrix.

## Validation plan

- Establish native-FTS relevance snapshots against all fixtures.
- Add an optional ParadeDB job (not required for ordinary pull requests) that provisions a supported `pg_search` release and runs the same query contract.
- Compare backend result ordering with deterministic ID tiebreaks; do not compare raw scores across algorithms.
- Test insert/update/delete synchronization and `VACUUM`; ParadeDB documents that dead rows can affect scores until vacuum removes them.[6]
- Review AGPL and redistribution implications before publishing any image or package that bundles ParadeDB Community.

## Sources

[1] https://github.com/paradedb/paradedb — ParadeDB repository
[3] https://www.postgresql.org/docs/14/textsearch-controls.html — PostgreSQL 14: Controlling Text Search
[4] https://www.postgresql.org/docs/17/textsearch-controls.html — PostgreSQL 17: Controlling Text Search
[6] https://docs.paradedb.com/documentation/sorting/score — ParadeDB relevance scoring
[7] https://docs.paradedb.com/welcome/architecture — ParadeDB architecture
[8] https://github.com/paradedb/paradedb/blob/dev/pg_search/README.md — pg_search development README
[9] https://github.com/paradedb/paradedb/blob/v0.8.0/pg_search/Cargo.toml — ParadeDB v0.8.0 pg_search Cargo features
[11] https://github.com/paradedb/paradedb/blob/v0.5.11/pg_bm25/README.md — Historical pg_bm25 README (v0.5.11)
[12] https://docs.paradedb.com/deploy/self-hosted/extension — ParadeDB self-hosted extension installation
[13] https://github.com/timescale/pg_textsearch — Timescale pg_textsearch repository
[14] https://github.com/tensorchord/VectorChord-bm25 — VectorChord-bm25 repository
