# Searching the catalog

This guide is for people and agents who **query** an already-loaded `pgokf`
catalog: how to write a search query, how results are ranked, how to walk the
link graph, and — most importantly — how to keep queries fast as a corpus grows
from thousands to tens of millions of concepts.

To *author* the concepts being searched, see the
[authoring guide](okf-authoring.md). For exact signatures, result columns, and
SQLSTATEs, see the [SQL API reference](sql-api.md). For measured native FTS
performance see [benchmarks](benchmarks.md); for the optional BM25 backend and
how to turn it on, see [Enabling the BM25 backend](#enabling-the-bm25-backend)
below.

All of the output below is **real**, captured from a live PostgreSQL 18 cluster
with the extension's `templates/` assembled into one bundle.

> **Roles.** `pgokf.concept_search` and `pgokf.concept_neighbors` require
> membership in `pgokf_reader` (or `pgokf_admin`, which inherits it). A fresh
> login role gets `42501` until it is `GRANT`ed one. See [security](security.md).

---

## Full-text search: `pgokf.concept_search`

```sql
pgokf.concept_search(
    query       text,
    bundle_id   bigint DEFAULT NULL,   -- NULL = all enabled bundles
    limit_count int    DEFAULT 20      -- 1..=500
) RETURNS SETOF pgokf.concept_search_result
```

The result rows are `(bundle_id, concept_id, path, title, type, rank,
headline)`. Note the search key is **`concept_id`**, and there is no `tags`
column — join `pgokf.concepts` to recover `tags`/`description`.

```sql
SELECT concept_id, type, round(rank::numeric, 4) AS rank
FROM pgokf.concept_search('payments restart')
LIMIT 10;
```
```
  concept_id  |   type   |  rank
--------------+----------+--------
 runbook      | runbook  | 0.9350
 incident     | incident | 0.1044
 skill        | skill    | 0.0700
 service      | service  | 0.0567
 wiki-article | article  | 0.0538
```

By default the engine is **native PostgreSQL full-text search** — no extensions
beyond `pgokf` are required, so it works on every supported server (PostgreSQL
15–19). Concretely, for each row:

- matching is `body_tsv @@ websearch_to_tsquery(<config>, query)`,
- ranking is `ts_rank_cd(body_tsv, query)`,
- the snippet is `ts_headline(<config>, title ‖ description ‖ body_text, query)`.

The **same function** can instead dispatch to a ParadeDB `pg_search` BM25
backend when the durable `search_backend` policy key is set to `bm25` — the
signature, result columns, and role checks are identical, so nothing in your
queries changes. See [Enabling the BM25 backend](#enabling-the-bm25-backend).

Only **enabled** bundles are searched. Pass `bundle_id` to scope to one bundle;
pass `limit_count` (1–500) to cap the result set.

### Query syntax (`websearch_to_tsquery`)

The `query` string uses PostgreSQL's `websearch_to_tsquery` grammar — the same
"web search box" syntax users already know:

| You write | Meaning |
| --------- | ------- |
| `payments restart` | both terms (implicit AND) |
| `"payments API"` | the exact phrase (adjacent terms) |
| `incident OR runbook` | either term |
| `payments -sev2` | `payments` but **not** `sev2` |

Unquoted words are AND-ed; `"…"` is a phrase; `OR` alternates; a leading `-`
negates. Syntax the user can't express (e.g. a raw `tsquery` `<->`) is simply
not reachable through this function — which is the point: untrusted input is
safe here because `websearch_to_tsquery` never errors on odd punctuation.

Phrase search, live:

```sql
SELECT concept_id, round(rank::numeric, 4) AS rank
FROM pgokf.concept_search('"payments API"');
```
```
  concept_id  |  rank
--------------+--------
 incident     | 1.9000
 runbook      | 1.7000
 service      | 1.2000
 wiki-article | 0.3000
 skill        | 0.2000
(5 rows)
```

All five concepts contain the adjacent phrase "payments API" — the incident,
runbook, and service score highest because the phrase appears in high-weight
fields, while the `wiki-article` and `skill` match it only in body text
(weight `D`), so they rank lower but are still returned.

Alternation:

```sql
SELECT concept_id, type FROM pgokf.concept_search('incident OR runbook');
```
```
  concept_id  |   type
--------------+----------
 runbook      | runbook
 incident     | incident
 service      | service
 skill        | skill
 wiki-article | article
```

Negation — because **tags are part of the search vector** (weight `B`), you can
exclude on a tag term. `sev2` is a tag only on the incident, so `payments -sev2`
drops exactly that concept:

```sql
SELECT concept_id, type FROM pgokf.concept_search('payments -sev2')
ORDER BY concept_id;
```
```
  concept_id  |  type
--------------+---------
 runbook      | runbook
 service      | service
 skill        | skill
 wiki-article | article
```

### Ranking

Ranking is **`ts_rank_cd`** — cover-density ranking, which rewards both
matched-lexeme frequency **and proximity**, so documents where the query terms
cluster together rank higher. It also honors the field weights baked into
`body_tsv` at index time:

| Weight | Field |
| ------ | ----- |
| `A` | `title` |
| `B` | `tags`, `type`, `description` |
| `D` | body text |

A title hit therefore far outranks a body hit for the same term. Results are
ordered `ORDER BY ts_rank_cd(...) DESC, concept_id ASC` — the `concept_id`
tiebreak makes equal-rank results **deterministic** across runs.

### Snippets (`ts_headline`)

`headline` is a `ts_headline` snippet computed over the concatenation of the
concept's `title`, `description`, and `body_text`, with matched terms wrapped in
`<b>…</b>`:

```sql
SELECT concept_id, headline FROM pgokf.concept_search('restart deploy') LIMIT 1;
```
```
concept_id | runbook
headline   | <b>restart</b> the payments API service after a failed <b>deploy</b>. <b>Restart</b> the payments API …
```

### Stemming

Matching runs through the configured text-search dictionary, so the query is
stemmed the same way the documents were. Under the default `english` config,
`restarting` matches documents that contain `restart`:

```sql
SELECT concept_id FROM pgokf.concept_search('restarting') ORDER BY concept_id;
-- incident, runbook, service, skill, wiki-article
```

The dictionary is the `default_text_search_config` policy key — see
[choosing the text-search configuration](#the-text-search-configuration-knob).

### Input validation

| Bad input | Result |
| --------- | ------ |
| empty / whitespace-only `query` | `ERROR: 22023: query must not be empty` |
| `limit_count` outside 1–500 | `ERROR: 22023: limit_count must be between 1 and 500, got 0` |

Both were captured live. `22023` is `invalid_parameter_value`.

### Structured filters (built in)

`concept_search` takes four optional trailing filters, each a no-op when `NULL`,
so the ranked hit set can be narrowed without a separate join. They are applied
as parameter-bound `AND` clauses inside the ranking query (reusing the `type`,
`tags`, and provenance indexes), so ranking still happens over the *filtered*
set:

```sql
pgokf.concept_search(
    query        text,
    bundle_id    bigint  DEFAULT NULL,
    limit_count  int     DEFAULT 20,
    concept_type text    DEFAULT NULL,  -- exact type match
    tags         text[]  DEFAULT NULL,  -- ALL-of: hit must carry every listed tag
    status       text    DEFAULT NULL,  -- concept_provenance.status
    trust_tier   text    DEFAULT NULL   -- concept_provenance.trust_tier
) RETURNS SETOF pgokf.concept_search_result
```

```sql
-- broad query, narrowed to human-reviewed runbooks tagged both 'payments' and 'oncall'
SELECT concept_id, round(rank::numeric, 4) AS rank
FROM pgokf.concept_search(
        'payments restart', NULL, 20,
        'Runbook', ARRAY['payments','oncall'], NULL, 'human-reviewed');
```

The tag filter is **ALL-of** (`tags @> filter`): a hit must carry *every* listed
tag. `status` and `trust_tier` match the concept's `pgokf.concept_provenance`
row, so a concept with no provenance frontmatter (no provenance row) is excluded
by a non-`NULL` `status`/`trust_tier` filter. The historical three-argument call
is unchanged.

---

## Selective vs. broad queries

This is the single most important thing to understand about search at scale.

- A **selective query** answers "find the few rows matching this predicate" —
  a point lookup by ID, a **tag** filter, a **type** filter, a scan of one
  small bundle. These ride B-tree / GIN indexes and stay **sub-millisecond to
  ~10–15 ms even at ~10M concepts**.
- A **broad "rank everything" query** asks the engine to score *every* matching
  row so it can return the global top-k. `ts_rank_cd` is evaluated **per row**,
  so its cost **scales linearly** with the size of the match set.

Measured on this project (see [benchmarks](benchmarks.md) and
[the FAQ](faq.md#when-does-native-fts-become-a-problem-and-what-does-bm25-buy)),
a broad ranked query over a common term costs about:

| Corpus size | Broad `ts_rank_cd` query |
| -----------:| ------------------------ |
| 1M concepts | ~322 ms |
| 10M concepts | ~2.4 s |
| 50M concepts | ~29 s |

That is the honest cost of ranking a match set that grows with the corpus. It
is fine for moderate corpora and interactive top-k over selective terms; it is
**not** fine when a single common term matches millions of rows.

### The pattern: pre-filter, then rank

The fix is to **shrink the match set with an indexed predicate before ranking
it**. Narrow by `bundle_id`, `type`, or `tags` first, so `ts_rank_cd` only ever
scores the survivors.

`pgokf.concept_search` has a built-in `bundle_id` filter — always use it when a
search is scoped to one bundle:

```sql
-- Ranked, but only within bundle 2, top 3.
SELECT concept_id FROM pgokf.concept_search('payments', 2, 3);
-- service, incident, wiki-article
```

For a **type** or **tag** pre-filter, query the base tables directly so the
planner can apply the btree / GIN index *before* ranking. This narrows to one
`type`, then ranks only that slice:

```sql
SELECT c.id,
       round(ts_rank_cd(c.body_tsv, q.query)::numeric, 4) AS rank
FROM pgokf.concepts c,
     websearch_to_tsquery('pg_catalog.english', 'payments') AS q(query)
WHERE c.bundle_id = 2
  AND c.type = 'runbook'          -- btree pre-filter
  AND c.body_tsv @@ q.query
ORDER BY rank DESC;
```
```
   id    |  rank
---------+--------
 runbook | 2.3000
```

> **Post-filtering `concept_search` is not the same thing.** Wrapping
> `concept_search(...)` in an outer `WHERE type = …` still makes the function
> rank the **whole** match set first, then discards rows — you pay the broad
> cost. Pre-filtering on the base tables lets the index cut the set down
> *before* `ts_rank_cd` runs. Reach for `concept_search` for convenience and
> selective queries; reach for a direct pre-filtered query when a broad term
> would otherwise score millions of rows.

Use the same `default_text_search_config` value in a hand-written query that the
catalog used to index the rows — read it with
`SELECT pgokf.get_config() ->> 'default_text_search_config'` (below).

---

## Filtering without ranking

Often you don't need ranking at all — you need "all concepts of this type" or
"everything tagged X". These ride dedicated indexes and are the fastest queries
in the catalog.

### Filter by type (btree)

`pgokf.concepts.type` has a btree index (`concepts_type_idx`):

```sql
SELECT id, title FROM pgokf.concepts WHERE bundle_id = 2 AND type = 'runbook';
```
```
   id    |          title
---------+--------------------------
 runbook | Restart the payments API
```

### Filter by tag (GIN)

`pgokf.concepts.tags` is a `text[]` with a GIN index (`concepts_tags_gin`). Use
array containment (`@>`) so the index is used:

```sql
SELECT id, title FROM pgokf.concepts
WHERE bundle_id = 2 AND tags @> ARRAY['oncall'];
```
```
   id    |             title
---------+-------------------------------
 runbook | Restart the payments API
 skill   | Operate the payments platform
```

`@> ARRAY['a','b']` requires **all** listed tags; use `&& ARRAY['a','b']` for
**any** of them.

### Filter by trust

`pgokf.concept_provenance.trust_tier` is btree-indexed, so you can gate results
behind a trust floor cheaply — e.g. "only human-reviewed concepts":

```sql
SELECT c.id, c.title, p.trust_tier
FROM pgokf.concepts c
JOIN pgokf.concept_provenance p
  ON p.bundle_id = c.bundle_id AND p.concept_id = c.id
WHERE p.trust_tier = 'human-reviewed';
```

See the [authoring guide](okf-authoring.md#derived-trust-tier) for how the tier
is derived. More filter/join recipes live in
[`examples/queries/search.sql`](https://github.com/LogicOcean/pgokf/blob/main/examples/queries/search.sql).

---

## The link graph

Walk the concept graph with **`pgokf.concept_neighbors`**:

```sql
pgokf.concept_neighbors(
    concept_id text,
    max_hops   int    DEFAULT 2,     -- >= 1, capped at pgokf.max_graph_hops
    bundle_id  bigint DEFAULT NULL
) RETURNS SETOF pgokf.concept_neighbor
```

It walks **resolved, non-external** internal edges (see
[links in the authoring guide](okf-authoring.md#links-and-the-concept-graph))
outward from a start concept, returning each reachable concept with its
**shortest** hop count and the path taken. Result rows are
`(source_id, neighbor_id, hops, path, title)`.

One hop from the `service` concept:

```sql
SELECT neighbor_id, hops, path FROM pgokf.concept_neighbors('service', 1, 2)
ORDER BY neighbor_id;
```
```
 neighbor_id  | hops |          path
--------------+------+------------------------
 incident     |    1 | {service,incident}
 runbook      |    1 | {service,runbook}
 wiki-article |    1 | {service,wiki-article}
```

Two hops reaches `skill` transitively (via `wiki-article`), and the shortest
path is kept:

```sql
SELECT neighbor_id, hops, path FROM pgokf.concept_neighbors('service', 2, 2)
ORDER BY hops, neighbor_id;
```
```
 neighbor_id  | hops |             path
--------------+------+------------------------------
 incident     |    1 | {service,incident}
 runbook      |    1 | {service,runbook}
 wiki-article |    1 | {service,wiki-article}
 skill        |    2 | {service,wiki-article,skill}
```

Properties worth knowing:

- The traversal is **cycle-safe**: it never revisits a concept already on the
  current path, so a link cycle cannot loop forever.
- Only **resolved** edges are followed — a broken internal link or an external
  URL is never a graph edge, and a neighbor whose concept was deleted is never
  emitted.
- `max_hops` must be **≥ 1** (`ERROR: 22023` otherwise) and is capped at the
  `pgokf.max_graph_hops` GUC ceiling.
- If `bundle_id` is omitted and the concept ID exists in **more than one
  bundle**, the call fails so you disambiguate — captured live:

  ```
  ERROR:  22023: concept_id 'service' exists in 2 bundles; pass bundle_id to disambiguate
  ```

To inspect the raw edges (including unresolved and external ones), query
`pgokf.links` directly — see
[`examples/queries/graph.sql`](https://github.com/LogicOcean/pgokf/blob/main/examples/queries/graph.sql).

---

## The text-search configuration knob

Which dictionary stems and tokenizes text is the durable
**`default_text_search_config`** policy key (default `pg_catalog.english`). It
drives **both** indexing and querying: `to_tsvector` when `body_tsv` is built at
sync time, and `websearch_to_tsquery` + `ts_headline` at query time — so query
parsing always matches the configuration that indexed the rows.

Read the effective value (captured live):

```sql
SELECT pgokf.get_config() ->> 'default_text_search_config' AS ts_config;
-- pg_catalog.english
```

Set it (admin only), e.g. to disable stemming with `simple`:

```sql
SELECT pgokf.set_config('default_text_search_config', '"pg_catalog.simple"'::jsonb);
```

> **⚠️ Changing it is not retroactive.** The config is read when each concept's
> `body_tsv` is built. Changing it does **not** re-tokenize already-indexed
> rows — they keep the vectors built under the old configuration, and a query
> parsed under the new one may mismatch them. Set
> `default_text_search_config` **before the first `register_bundle`**; to change
> it on an existing catalog, re-index by re-registering the affected bundles.
> Full detail — including the value's validation against
> `pg_catalog.pg_ts_config` — is in
> [configuration](configuration.md#which-keys-the-current-engine-consults).

---

## Enabling the BM25 backend

Native `ts_rank_cd` is the shipped, zero-dependency **default**, and it is the
right choice for selective queries and moderate corpora. For **broad,
relevance-ranked** queries — where a common term matches millions of rows and
native ranking scales linearly (see
[Selective vs. broad queries](#selective-vs-broad-queries)) — `pgokf` can
instead run **BM25 top-k** over a ParadeDB `pg_search` index. Block-Max WAND
pruning keeps broad queries roughly **flat** where native grows linearly, while
native stays the winner for selective ones. The `pg_search` version matrix and
the `search_backend` key are covered in
[configuration](configuration.md#search-backend-search_backend); native's
measured numbers are in [benchmarks](benchmarks.md).

The BM25 backend is **opt-in** and rides on an external extension, so it is off
until you enable it deliberately.

### Honesty first — the dependency

`native` is the default precisely because it needs nothing beyond `pgokf`. BM25
requires **ParadeDB `pg_search`**, which:

- is licensed **AGPL-3.0** (Community edition) — evaluate that against your
  distribution model before adopting it;
- must be added to **`shared_preload_libraries`** (a server restart), and
  requires **`pgvector`** installed alongside it;
- targets PostgreSQL **15–18** and is **not** available on every managed
  PostgreSQL service.

`pgokf` itself never links `pg_search`: `CREATE EXTENSION pgokf` succeeds with or
without it, and the code reaches every `pg_search` object only through runtime
SQL. If you cannot take that dependency, stay on `native` — nothing else in
`pgokf` needs it.

### Steps

1. **Install `pg_search` (and `pgvector`) at the cluster level.** Add
   `pg_search` to `shared_preload_libraries` in `postgresql.conf` and restart:

   ```conf
   # postgresql.conf
   shared_preload_libraries = 'pg_search'
   ```

   Then, in the database that holds the catalog, create the extensions
   (`CASCADE` pulls in `vector`):

   ```sql
   CREATE EXTENSION IF NOT EXISTS pg_search CASCADE;
   ```

2. **Switch the backend** (admin only):

   ```sql
   SELECT pgokf.set_config('search_backend', '"bm25"'::jsonb);
   ```

3. **Build the index** with the admin-only function (captured live):

   ```sql
   SELECT pgokf.rebuild_search_index();
   -- t
   ```

   `rebuild_search_index()` (re)creates a `bm25` index on `pgokf.concepts` over
   `id` (the key field), `title`, `description`, `body_text`, and `type`. It is
   idempotent — safe to re-run — and returns `false` with a `NOTICE` when
   `pg_search` is not installed (a no-op). Once the index exists, ordinary
   incremental sync (`register_bundle` / `refresh_bundle`) maintains it
   automatically; re-run `rebuild_search_index()` only if you want it rebuilt
   from scratch.

That's it — `pgokf.concept_search('database')` now returns BM25-ranked results
with `paradedb.score` in the `rank` column, still carrying a `ts_headline`
snippet so the `headline` column is unchanged.

### Graceful fallback

If `search_backend` is `bm25` but the prerequisites are missing, search **does
not error** — it falls back to native and logs a `WARNING`, so a half-finished
setup degrades instead of breaking:

```sql
-- search_backend = 'bm25', but pg_search is not installed:
SELECT concept_id FROM pgokf.concept_search('database');
-- WARNING:  pgokf: search_backend is 'bm25' but the pg_search extension is not
--           installed; falling back to native full-text search. ...
--  (native results follow)
```

The same fallback (with a "no bm25 index" warning) happens when `pg_search` is
installed but `rebuild_search_index()` has not been run yet. To silence the
warning, either finish the setup or set `search_backend` back to `native`.

### Tokenizer differences to expect

The two backends tokenize differently, so a query can rank — or match — slightly
differently between them. Native FTS applies the `default_text_search_config`
dictionary (English stemming by default), so `postgres` stems to match
`PostgreSQL`. The BM25 index uses `pg_search`'s default tokenizer, which
lowercases but does not apply that stemmer, so the literal term `postgres` will
**not** match `postgresql`; search for the term as it appears in the text
(`database`, `failover`, …). This is expected, not a bug: BM25 is tuned for
broad relevance ranking, native for dictionary-faithful matching.

Keep broad queries fast on native, when you are not on BM25, with the
[pre-filter-then-rank pattern](#the-pattern-pre-filter-then-rank) above.

---

## Content similarity: `pgokf.find_similar`

`find_similar(concept_id, bundle_id, limit_count)` answers "what else reads like
this one?" — content similarity, **not** the authored link graph
(`concept_neighbors`). It extracts the seed concept's most salient `body_tsv`
lexemes (highest term frequencies), runs them as an `OR` query through the
configured `search_backend` (native FTS or BM25), and excludes the seed itself.

```sql
SELECT concept_id, round(rank::numeric, 4) AS rank
FROM pgokf.find_similar('runbooks/database-failover');
```

Because it dispatches through the same backend seam as `concept_search`, turning
on the BM25 backend makes `find_similar` a BM25 more-like-this automatically. If
the seed id exists in more than one bundle, pass `bundle_id` to disambiguate
(otherwise `22023`).

---

## Semantic and hybrid search (optional, pgvector)

For "find things that *mean* the same" — where the words differ but the meaning
matches — `pgokf` offers an optional **semantic** surface backed by
[`pgvector`](https://github.com/pgvector/pgvector), and a **hybrid** surface that
fuses lexical and semantic ranking. Both are opt-in and, exactly like the BM25
backend, add **no static dependency**: `CREATE EXTENSION pgokf` succeeds without
pgvector, and the `pgokf.concept_embedding` table stores vectors as the builtin
`real[]`, cast to `vector` only at query and index time.

### The embedding companion (how vectors get in)

`pgokf` **never computes embeddings and never does network I/O.** Embeddings are
produced by *your* embedder — the same mountless-companion pattern as
[`pgokf-ingest`](https://github.com/LogicOcean/pgokf/tree/main/crates/pgokf-ingest):
a process you run computes each concept's vector (from its `body_text`, which you
can read with `pgokf.get_concept_source` or from your own source of truth) and
streams it in as `pgokf_writer`:

```sql
-- one row per concept, from your embedder
SELECT pgokf.set_concept_embedding(1, 'runbooks/database-failover',
                                   ARRAY[0.0123, -0.0456, ...]::real[]);
```

Set `embedding_dim` to match your model first (default `1536`):

```sql
SELECT pgokf.set_config('embedding_dim', '768'::jsonb);   -- admin
```

`set_concept_embedding` rejects any vector whose length differs from
`embedding_dim` (`22023`). After a bulk load, build the ANN index:

```sql
SELECT pgokf.rebuild_embedding_index();   -- admin; pgvector HNSW cosine
```

### Semantic search

```sql
-- query_embedding is your query text run through the SAME embedder
SELECT concept_id, round(rank::numeric, 4) AS cosine_similarity
FROM pgokf.concept_search_semantic(ARRAY[0.0201, -0.0388, ...]::real[]);
```

The `rank` column is the normalized cosine similarity (`1.0` for an identical
vector). **Semantic search requires pgvector**: because it has no lexical
equivalent, it raises `22023` naming the missing dependency (`CREATE EXTENSION
vector`) when pgvector is absent — never a silent empty result.

### Hybrid search (RRF)

Hybrid fuses the lexical result of a text `query` (through the configured
`search_backend`) with the semantic result of a `query_embedding`, using
**Reciprocal Rank Fusion** (RRF, k = 60), entirely in SQL — no model is involved
in the fusion itself:

```sql
SELECT concept_id, round(rank::numeric, 6) AS rrf
FROM pgokf.concept_search_hybrid('database failover',
                                 ARRAY[0.0201, -0.0388, ...]::real[]);
```

RRF sums `1 / (60 + rank)` across the two lists, so a concept that ranks well in
*both* the lexical and semantic lists outranks one strong in only one — the
common case where a query is strong lexically *and* semantically. When pgvector
is absent, hybrid **degrades to lexical-only with a `WARNING`** (unlike pure
semantic search, a lexical-only answer is still sensible).

> **Which surface when?** Use `concept_search` (optionally with BM25) for keyword
> and filtered search; `find_similar` for "more like this document";
> `concept_search_semantic` for meaning-based recall where wording differs; and
> `concept_search_hybrid` when you want the best of lexical precision and semantic
> recall in one ranked list.

---

## See also

- [SQL API reference](sql-api.md) — full signatures, result columns, SQLSTATEs.
- [Authoring guide](okf-authoring.md) — how the fields you search on get there.
- [Configuration](configuration.md) — `default_text_search_config` and the GUC
  ceilings (`pgokf.max_graph_hops`, and more).
- [Benchmarks](benchmarks.md) — measured FTS / filter / graph performance.
- [Configuration](configuration.md#search-backend-search_backend) — the
  `search_backend` key and the `pg_search` version matrix for the BM25 backend.
- Example queries: [`examples/queries/search.sql`](https://github.com/LogicOcean/pgokf/blob/main/examples/queries/search.sql),
  [`examples/queries/graph.sql`](https://github.com/LogicOcean/pgokf/blob/main/examples/queries/graph.sql).
