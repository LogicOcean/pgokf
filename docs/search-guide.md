# Searching the catalog

This guide is for people and agents who **query** an already-loaded `pgokf`
catalog: how to write a search query, how results are ranked, how to walk the
link graph, and — most importantly — how to keep queries fast as a corpus grows
from thousands to tens of millions of concepts.

To *author* the concepts being searched, see the
[authoring guide](okf-authoring.md). For exact signatures, result columns, and
SQLSTATEs, see the [SQL API reference](sql-api.md). For the ranking research and
the future BM25 adapter, see [BM25 research](bm25-research.md) and
[benchmarks](benchmarks.md).

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

The engine is **native PostgreSQL full-text search** — no extensions beyond
`pgokf` are required, so it works on every supported server (PostgreSQL 15–19).
Concretely, for each row:

- matching is `body_tsv @@ websearch_to_tsquery(<config>, query)`,
- ranking is `ts_rank_cd(body_tsv, query)`,
- the snippet is `ts_headline(<config>, title ‖ description ‖ body_text, query)`.

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

## BM25 is a future adapter, not a current function

There is **no `pgokf` BM25 function today.** Native `ts_rank_cd` is the shipped,
required baseline, and it is the right default for selective queries and
moderate corpora.

BM25 is a **benchmarked, future, optional adapter**. The research — a ParadeDB
`pg_search` top-k adapter — kept broad "rank everything" queries roughly **flat
at ~10–15 ms** where native `ts_rank_cd` scales linearly, a **30–194×** speedup
on broad queries, by pruning with WAND-style top-k instead of scoring the whole
match set. It is not shipped because it is not available on arbitrary managed
PostgreSQL and carries licensing/operational cost. The full analysis, the
supported-version matrix, and the validation plan are in
[BM25 research](bm25-research.md); the measured native-FTS numbers are in
[benchmarks](benchmarks.md).

Until then: keep broad queries fast with the
[pre-filter-then-rank pattern](#the-pattern-pre-filter-then-rank) above.

---

## See also

- [SQL API reference](sql-api.md) — full signatures, result columns, SQLSTATEs.
- [Authoring guide](okf-authoring.md) — how the fields you search on get there.
- [Configuration](configuration.md) — `default_text_search_config` and the GUC
  ceilings (`pgokf.max_graph_hops`, and more).
- [Benchmarks](benchmarks.md) — measured FTS / filter / graph performance.
- [BM25 research](bm25-research.md) — the future top-k adapter.
- Example queries: [`examples/queries/search.sql`](https://github.com/LogicOcean/pgokf/blob/main/examples/queries/search.sql),
  [`examples/queries/graph.sql`](https://github.com/LogicOcean/pgokf/blob/main/examples/queries/graph.sql).
