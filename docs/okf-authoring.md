# Authoring OKF concepts

This guide is for the people and agents who **write** the Markdown that `pgokf`
ingests: how to shape a concept's YAML frontmatter, which keys the catalog
recognizes, and exactly where every field lands once a bundle is synced. It
targets **OKF v0.2**, the version this extension conforms to.

If you only want to *query* an already-loaded catalog, see the
[search guide](search-guide.md) instead. For the authoritative SQL surface —
function signatures, table columns, SQLSTATEs — see the
[SQL API reference](sql-api.md). For the durable policy knobs referenced below
(`store_source`, `default_text_search_config`, `default_strict`,
`default_exclude`) see [configuration](configuration.md).

> **Spec.** OKF v0.2 is defined by the `okf/SPEC.md` document in
> [`GoogleCloudPlatform/knowledge-catalog`](https://github.com/GoogleCloudPlatform/knowledge-catalog).
> Where pgokf is deliberately more permissive or slightly stricter than the
> spec, this page says so explicitly.

Every example below is drawn from the copy-and-fill starters in the
repository's [`templates/`](https://github.com/LogicOcean/pgokf/blob/main/templates/) directory — start there.

---

## What a concept file is

An OKF **concept** is a single Markdown file:

```markdown
---
type: runbook
title: Restart the payments API
description: Recover the payments API after a bad deploy.
tags:
  - oncall
  - payments
---

# Restart the payments API

The body is ordinary Markdown. Links to sibling files become graph edges.
```

Two parts, in this order:

1. A **YAML frontmatter block**, delimited by a line that is exactly `---`
   above and a line that is exactly `---` below. The opening `---` must be the
   very first line of the file (a leading UTF-8 BOM is tolerated). A file that
   does not begin with the delimiter is rejected with a parse error
   (`Markdown file must begin with a YAML frontmatter delimiter`).
2. A **Markdown body**, everything after the closing delimiter. It is stored
   as `pgokf.concepts.body_text` and indexed for full-text search.

> **The closing `---` is line-based.** pgokf finds the first line whose entire
> content is `---` and treats it as the close — it does *not* run a full YAML
> parser to find the boundary. A bare `---` inside a multiline quoted scalar
> will therefore cut the block short. Keep such values on one line, or use a
> YAML block scalar (`|` / `>`).

### The concept ID comes from the path, not the frontmatter

A concept's identity is its **bundle-relative path without the `.md` suffix**,
normalized:

| File in the bundle | Concept ID |
| ------------------ | ---------- |
| `payments-api.md` | `payments-api` |
| `runbooks/failover.md` | `runbooks/failover` |

You **may** put an `id:` in frontmatter — it is recorded for diagnostics but is
never used as the key. Rename the file to rename the concept. This is why the
templates cross-link by filename: the link target *is* the target's ID plus
`.md`.

---

## The frontmatter contract

pgokf recognizes five **modeled fields** and treats everything else as
producer metadata. This is enforced in the parser
(`crates/okf-parser/src/frontmatter.rs`) and the catalog projection
(`crates/extension/src/catalog/`).

| Key | Required? | Type | Projects to |
| --- | --------- | ---- | ----------- |
| `type` | **Yes** | string (free-form) | `pgokf.concepts.type` (btree indexed) |
| `title` | **Yes, in pgokf** (see note) | string | `pgokf.concepts.title` (search weight **A**) |
| `description` | Recommended | string | `pgokf.concepts.description` (search weight **B**) |
| `tags` | Recommended | list of strings | `pgokf.concepts.tags` `text[]` (GIN indexed, search weight **B**) |
| `resource` | Recommended | any YAML value | `pgokf.concepts.resource`, stored verbatim as JSON |
| *any other key* | — | any | one row in `pgokf.concepts` → `pgokf.concept_metadata` (see [Custom metadata](#custom-metadata)) |

### `type` — required and free-form

`type` is the only field OKF v0.2 makes strictly required, and it is an
**open string**: `runbook`, `service`, `incident`, `article`, `skill`,
`Attested Computation`, or anything your producer defines. Consumers tolerate
unknown types. pgokf stores it verbatim and indexes it with a btree
(`concepts_type_idx`) so `WHERE type = '…'` filters are cheap — see
[filtering by type](search-guide.md#filter-by-type-btree).

### `title` — required by pgokf's parser

> **Implementation note — stricter than the spec.** OKF v0.2 lists `title` as
> *recommended*, not required. pgokf's current parser is slightly stricter: it
> requires a non-empty `title` on **every** concept document. In
> `crates/okf-parser/src/frontmatter.rs`, `title` is a non-optional field, so a
> concept whose frontmatter omits it fails to parse — and under the default
> strict sync policy that aborts the whole `register_bundle` /
> `refresh_bundle`. In practice: **always give every concept a `title`.** It is
> also the highest-weighted (`A`) search field, so it is worth authoring well.

### `description`, `tags`, `resource`

- **`description`** — a short human summary. Indexed at search weight `B` and
  included in `ts_headline` snippets.
- **`tags`** — a YAML list of strings, kept in declaration order as a
  `text[]`. Backed by a GIN index (`concepts_tags_gin`) for fast containment
  filters (`tags @> ARRAY['oncall']`) and also folded into the search vector at
  weight `B`. Use tags as your primary **pre-filter** dimension for large
  corpora (see the [search guide](search-guide.md#selective-vs-broad-queries)).
- **`resource`** — OKF recommends a URI pointing at the real thing the concept
  models (a service URL, a dataset, a document). pgokf stores whatever YAML you
  give it, converted to JSON, verbatim in `pgokf.concepts.resource`. A bare
  string round-trips as a JSON string:

  ```yaml
  resource: https://payments.internal.example.test
  ```
  ```
   id    |                 resource
  -------+------------------------------------------
   service | "https://payments.internal.example.test"
  ```

---

## Provenance, trust, and lifecycle

OKF v0.2 defines three families of metadata that describe **where a concept
came from, how far it can be trusted, and when it goes stale**. pgokf lifts
these into typed columns across three tables, derives a trust tier, and *also*
keeps the raw keys losslessly. A concept that carries **none** of these keys
gets no provenance row at all — the projection is **sparse**.

The recognized key set (from `crates/extension/src/catalog/provenance.rs`) is:

```
generated   generated_at   generated_by   verified
sources     usage_window   stale_after    status
```

### The actor convention

Every "who did this" value across the families uses one convention:

| Actor kind | Form | Example |
| ---------- | ---- | ------- |
| An agent / tool | `<producer>/<version>` | `sre-agent/1.4` |
| A person | `human:<id>` | `human:platform-lead` |
| A process / pipeline | `process:<id>` | `process:service-catalog-sync` |

The `human:` prefix is load-bearing: it is what promotes a concept's derived
[trust tier](#derived-trust-tier) to `human-reviewed`.

### `generated` — origin

Who produced the *current* content, and when:

```yaml
generated:
  by: sre-agent/1.4
  at: 2026-08-01T09:30:00Z
```

- `generated.by` → `pgokf.concept_provenance.generated_by`
- `generated.at` → `pgokf.concept_provenance.generated_at` (parsed from ISO 8601)

pgokf tolerates two shorthands: a bare `generated_by:` / `generated_at:` scalar,
or a bare string `generated: sre-agent/1.4` (treated as the `by`).

### `verified` — a **list** of verification events

`verified` is an **ordered list** of `{by, at}` events — a concept can be
verified more than once, by different actors, over time:

```yaml
verified:
  - by: sre-agent/1.4
    at: 2026-08-01T09:35:00Z
  - by: human:sre-lead
    at: 2026-08-02T14:30:00Z
```

Each event becomes one row in **`pgokf.concept_verification`**, keyed by a
zero-based `ordinal` that preserves list order:

```
 concept_id | ordinal |    verified_by     |      verified_at
------------+---------+--------------------+------------------------
 runbook    |       0 | human:sre-lead     | 2026-08-02 14:30:00+00
```

Rules pgokf applies:

- A **single mapping** (not a list) is accepted and stored as one `ordinal = 0`
  row — OKF v0.2 treats a lone verified mapping as a one-element list.
- An event with **no `by` actor is skipped** (the `verified_by` column is
  `NOT NULL`); it leaves a gap in the ordinals rather than renumbering peers.
- `at` is parsed from ISO 8601; an absent or unparseable `at` stores `NULL`.
- **Omit `verified` entirely** for an unverified draft.

### Derived trust tier

pgokf derives `pgokf.concept_provenance.trust_tier` from the `verified[]`
actors — you never write it yourself:

| Tier | Condition |
| ---- | --------- |
| `unverified` | no verification events |
| `machine-confirmed` | ≥ 1 event, but no `human:` actor |
| `human-reviewed` | at least one event whose actor starts with `human:` |

Verified live against the templates plus an agent-verified fixture:

```
  concept_id  |         generated_by         |   trust_tier
--------------+------------------------------+-------------------
 wiki-article | platform-docs-agent/1.0      | unverified
 etl-report   | etl-agent/2.1                | machine-confirmed
 runbook      | sre-agent/1.4                | human-reviewed
```

`trust_tier` is btree-indexed (`concept_provenance_trust_tier_idx`), so you can
cheaply gate a search behind a trust floor — e.g. "only human-reviewed
concepts."

### `sources` — the materials the content was derived from

`sources` is a list of the inputs the concept was built from. Each entry maps
to one row of **`pgokf.concept_provenance_source`** (ordinal-keyed):

```yaml
usage_window:
  from: 2026-07-01T00:00:00Z
  to: 2026-07-15T00:00:00Z
sources:
  - id: service-catalog
    resource: https://catalog.internal.example.test/services/payments-api
    title: Service catalog entry
    author: process:service-catalog-sync
    usage_count: 1
    last_modified: 2026-07-14T00:00:00Z
```

Per-entry fields → columns: `id` → `source_id`, `resource`, `title`, `author`,
`usage_count` (coerced from a number or numeric string), `last_modified`
(ISO 8601). An entry may carry its own `usage_window: {from, to}` that overrides
the top-level window; both project to `usage_window_from` / `usage_window_to`.

> **Lenient by design.** OKF requires `resource` on each source entry, but
> pgokf stores it leniently (`NULL` when absent) so one malformed source never
> aborts a sync. Non-object entries in the list are skipped.

`pgokf.concept_provenance_source` (provenance *materials*) is distinct from
`pgokf.concept_source` (the concept's own raw bytes, the `store_source` tier —
see [storage tiers](#storage-tiers-store_source)).

### `usage_window`, `status`, `stale_after` — lifecycle

- **`usage_window: {from, to}`** (top level) frames the window all source
  `usage_count`s are counted within → `concept_provenance.usage_window_from` /
  `usage_window_to`.
- **`status`** — the lifecycle state, `draft | stable | deprecated` →
  `concept_provenance.status`. Per the spec, an **absent** status means
  `stable`; pgokf stores `NULL` for absent and leaves that default to the
  reader.
- **`stale_after`** — an absolute ISO 8601 instant after which the content is
  considered stale → `concept_provenance.stale_after`.

### The lossless `details` column

Every recognized provenance key is *also* copied verbatim, as JSON, into
`pgokf.concept_provenance.details`. The typed columns give you fast, indexable
access; `details` guarantees nothing in the recognized subset is lost to the
projection.

### Timestamps are defensive

Every timestamp above is parsed by a restricted ISO 8601 / RFC 3339 reader
(`YYYY-MM-DD`, optional `T`/space + `HH:MM[:SS[.fff]]`, optional `Z` / `±HH:MM`
zone; a bare date is midnight UTC, a naive time is UTC). Every calendar field is
range-checked, so an impossible instant like `2026-02-30` — or any malformed
value — projects a SQL `NULL` and **never aborts the sync**. The raw text still
survives in `concept_metadata` and (for recognized keys) in `details`.

---

## Custom metadata

**Every** frontmatter key that is not one of the five modeled fields
(`type` / `title` / `description` / `tags` / `resource`) becomes one row in
**`pgokf.concept_metadata`** — key plus value as compact JSON. Structured
values survive intact:

```yaml
owner: team-payments
proficiency_levels:
  - novice
  - practitioner
  - expert
required_for_role: payments-oncall
review_cadence_days: 180
```
```
         key         |                       value
---------------------+---------------------------------------------------
 owner               | "team-payments"
 proficiency_levels  | ["novice", "practitioner", "expert"]
 required_for_role   | "payments-oncall"
 review_cadence_days | 180
```

`concept_metadata.value` is GIN-indexed (`jsonb_path_ops`), so containment
queries over producer metadata are cheap.

> **Provenance keys appear in *both* places.** The recognized provenance /
> trust / lifecycle keys are *not* removed from `concept_metadata` — they land
> there as raw metadata **and** are projected into the provenance tables. So
> `generated`, `verified`, `sources`, `usage_window`, `stale_after`, and
> `status` are queryable both as raw JSON (in `concept_metadata`) and as typed
> columns (in the provenance tables). Verified live: a `skill` concept shows
> `generated`, `verified`, `stale_after`, and `status` in `concept_metadata`
> alongside its purely-custom `owner` / `proficiency_levels` keys.

### `skill` has no spec-mandated fields

`type: skill` is just a producer-chosen type string. **OKF defines no
skill-specific fields.** Everything a skill concept carries beyond the five
modeled fields and the provenance families is producer metadata in
`concept_metadata`. The same is true of `runbook`, `service`, `incident`,
`article`, and every other type — with exactly **one** exception below.

---

## The one special type: `Attested Computation`

`Attested Computation` is the **only** OKF v0.2 type with spec-mandated
type-specific fields. It records a computation that was run and attested:

```yaml
---
type: Attested Computation
title: Nightly revenue rollup
tags: [finance, etl]
runtime: python-3.12
parameters:
  window: 2026-08-01/2026-08-27
computation: sum(amount) group by day
executor: process:airflow/2.9
attester: attestation-agent/3.0
generated:
  by: etl-agent/2.1
  at: 2026-08-27T02:00:00Z
verified:
  - by: attestation-agent/3.0
    at: 2026-08-27T02:05:00Z
status: stable
---
```

The type-specific fields are `runtime`, `parameters`, `computation`,
`executor`, and `attester`.

> **`runtime` and `parameters` get no dedicated columns.** They are not
> provenance keys, so they are retained as producer metadata in
> `concept_metadata` — exactly like any other non-modeled key. `generated` /
> `verified` / `status` additionally populate the provenance tables (the example
> above yields a `machine-confirmed` trust tier, since the sole verifier is an
> agent, not a `human:` actor). No other type — `skill` included — has
> spec-mandated fields.

### `computation` / `executor` / `attester` become graph edges

The three **reference-bearing** type-specific fields point at other concepts,
so pgokf resolves them into `pgokf.links` as typed, traversable edges — the same
resolution ordinary Markdown links get. Each field may be written as a bare
resource path or as a `{resource: …}` mapping (the shape the OKF v0.2 spec uses
to attach a `receipt`):

```yaml
---
type: Attested Computation
title: Monthly active accounts
computation: /computation.md
executor:
  resource: /executor.md
  receipt: [query_id, executed_sql, result]
attester:
  resource: /attester.md
---
```

Registering this projects three edges from the concept, each carrying a
`link_relation` of `attestation:computation`, `attestation:executor`, or
`attestation:attester`. They are numbered after the concept's body links and
resolve exactly like an internal Markdown link — an internal reference to a
concept in the bundle is `resolved = true` and is **traversed by
`pgokf.concept_neighbors`**, while an external or dangling reference is retained
as `is_external` / `resolved = false` and never traversed. Verified live, the
concept above reaches its `computation`, `executor`, and `attester` concepts even
though its body links to none of them:

```
 source_id | target_id   |      link_relation      | resolved | is_external
-----------+-------------+-------------------------+----------+-------------
 rich      | computation | attestation:computation | t        | f
 rich      | executor    | attestation:executor    | t        | f
 rich      | attester    | attestation:attester    | t        | f
```

Only the `Attested Computation` type resolves these keys; on any other type the
same keys stay ordinary `concept_metadata`.

---

## Links and the concept graph

Ordinary Markdown links in the body become directed edges in
**`pgokf.links`**, one row per outgoing link, in document order:

```markdown
Operational runbook for the [payments API service](service.md). Escalate by
[emailing the on-call lead](mailto:oncall@example.test).
```

- **Internal links** (`[label](service.md)`, `[label](https://github.com/LogicOcean/pgokf/blob/main/runbooks/failover.md)`)
  resolve to another concept. The target path is normalized relative to the
  **source file's directory**, and `.md` is appended when the destination has
  no extension. The resolved concept ID is `target_path` without `.md`.
- **External links** (`https:`/`mailto:` and other scheme-qualified or
  protocol-relative URLs) carry `target_id = NULL`, `is_external = true`, and
  never become graph edges.

Each edge records `link_kind` — the Markdown construct that produced it:
`inline`, `reference`, `autolink`, `email`, or `image`.

### `resolved` is recomputed bundle-wide

An internal edge is `resolved = true` only when its `target_id` matches a
concept that exists **in the same bundle**. pgokf recomputes `resolved` across
the whole bundle after every sync, so adding a target flips its inbound edges
`true` and removing a target flips them `false`. Broken internal links are
retained (OKF permits them) but stay `resolved = false`. Live, for the
`service` concept:

```
 source_id |  target_id   |           link_text            | resolved | is_external
-----------+--------------+--------------------------------+----------+-------------
 service   | wiki-article | payments architecture overview | t        | f
 service   | runbook      | restart the payments API       | t        | f
 service   |              | page the on-call lead          | f        | t
```

Only **resolved, non-external** edges are walked by `pgokf.concept_neighbors` —
see [the graph section of the search guide](search-guide.md#the-link-graph).

### Linking conventions

- Prefer **relative** links between siblings and subdirectories
  (`service.md`, `runbooks/failover.md`, `../architecture.md`). They work in a
  plain Markdown viewer *and* resolve as graph edges.
- The target's concept ID is its path minus `.md`, so a link to
  `runbooks/failover.md` resolves to the concept `runbooks/failover`.
- The [templates](https://github.com/LogicOcean/pgokf/blob/main/templates/) cross-link each other by filename, so a
  bundle assembled from them exercises the link graph out of the box.

---

## Reserved files: `index.md` and `log.md`

Two filenames are reserved at **every** directory level and are **never**
ingested as concepts:

| File | Purpose |
| ---- | ------- |
| `index.md` | Describes a directory (or the bundle, at the root). |
| `log.md` | Chronological history for its directory. |

The **bundle-root `index.md`** is special in one way: its frontmatter may carry
an optional **`okf_version`**, which pgokf reads and stores on
`pgokf.bundles.okf_version` for that bundle:

```markdown
---
okf_version: "0.2"
---

# Payments knowledge base

Free-form overview for humans. Only `okf_version` above is read by the catalog.
```

Both `okf_version: "0.2"` and the unquoted `okf_version: 0.2` are accepted. An
absent or malformed value leaves `bundles.okf_version` `NULL` and never aborts
a sync. Everything else in an `index.md`'s frontmatter and body is ignored by
the parser — use the body freely for a table of contents or overview.

Verified live: a bundle whose root `index.md` declares `okf_version: "0.2"`
registers with:

```
 id |   name   | okf_version | file_count
----+----------+-------------+------------
  2 | payments | 0.2         |          5
```

(The five concepts are the non-reserved `.md` files; `index.md` is not one of
them.)

### `log.md` is projected as a per-directory activity log

Unlike `index.md`, a `log.md` **is** projected — into `pgokf.bundle_log`, one
row per entry — while still never becoming a concept. On every sync each
`log.md` in the bundle is parsed line by line: a leading ISO 8601 timestamp
(after any list bullet or heading marker) is lifted into `logged_at`, and the
trimmed line is stored losslessly in `entry`. Blank lines are skipped. Write it
as an ordinary Markdown log:

```markdown
# Activity

- 2026-07-01T12:00:00Z Registered the bundle
- 2026-07-02T09:30:00Z Refreshed after an edit
Freeform note without a timestamp
```

Read a directory's log with `pgokf.list_bundle_log(bundle_id[, directory])`; the
`directory` column is the empty string for a root-level `log.md`. The projection
is replaced wholesale on every sync, so it tracks edits, additions, and removals
of the files, and a bundle with no `log.md` simply has no rows. Verified live,
the log above projects (ordered by directory, then ordinal):

```
 directory | ordinal |       logged_at        |                    entry
-----------+---------+------------------------+----------------------------------------------
           |       0 |                        | # Activity
           |       1 | 2026-07-01 12:00:00+00 | - 2026-07-01T12:00:00Z Registered the bundle
           |       2 | 2026-07-02 09:30:00+00 | - 2026-07-02T09:30:00Z Refreshed after an edit
           |       3 |                        | Freeform note without a timestamp
```

---

## How the whole file projects — at a glance

| Frontmatter / body | Lands in |
| ------------------ | -------- |
| `type`, `title`, `description` | columns on `pgokf.concepts` |
| `tags` (list) | `pgokf.concepts.tags` `text[]` (GIN) |
| `resource` | `pgokf.concepts.resource` (verbatim JSON) |
| Markdown body | `pgokf.concepts.body_text` + weighted `body_tsv` |
| Markdown links | `pgokf.links` (internal resolve; external flagged) |
| `Attested Computation` `computation` / `executor` / `attester` | typed edges in `pgokf.links` (`link_relation = attestation:*`) **and** `concept_metadata` |
| **any non-modeled key** | one row in `pgokf.concept_metadata` (JSON) |
| `generated` / `status` / `stale_after` / top-level `usage_window` | `pgokf.concept_provenance` (scalar) + `details` **and** `concept_metadata` |
| `verified[]` | `pgokf.concept_verification` (one row per event) **and** `concept_metadata` |
| `sources[]` | `pgokf.concept_provenance_source` (one row per entry) **and** `concept_metadata` |
| derived trust tier | `pgokf.concept_provenance.trust_tier` |
| bundle-root `index.md` `okf_version` | `pgokf.bundles.okf_version` |
| per-directory `log.md` entries | `pgokf.bundle_log` (one row per entry) |

The search vector `body_tsv` is weighted: **title `A`**, **tags / type /
description `B`**, **body `D`** — which is why the same query term ranks a title
hit far above a body hit (see the [search guide](search-guide.md#ranking)).

---

## Bundle-level authoring concerns

### Malformed files and the strict policy

Sync behavior on a malformed file is governed by the durable **`default_strict`**
policy (see [configuration](configuration.md)):

- **`default_strict = true` (default)** — the **first** malformed file aborts
  the whole `register_bundle` / `refresh_bundle` transaction. Nothing is
  written. This is the safe default: you learn about a broken file immediately.
- **`default_strict = false`** — a malformed file is logged as a warning and
  **skipped**, and the rest of the bundle registers. A file that cannot be
  *read* (an I/O error, not a parse error) still aborts even in this mode.

A common gotcha: a stray `README.md` with no frontmatter in the bundle root
will, under the strict default, abort the sync
(`… must begin with a YAML frontmatter delimiter …`). Keep non-concept prose
out of the bundle, or in a reserved `index.md` / `log.md`, or exclude it with
[`default_exclude`](configuration.md).

### Size limits

Three GUC ceilings bound what the parser will accept (see
[configuration](configuration.md)): `pgokf.max_file_bytes` (whole file),
`pgokf.max_frontmatter_bytes` (the YAML block), and `pgokf.max_bundle_files`
(files scanned per bundle). These are safety ceilings, not per-concept policy.

### Storage tiers (`store_source`)

The durable `store_source` key decides whether the concept's **verbatim source
bytes** are kept in `pgokf.concept_source`:

- **`store_source = false` (default)** — files live in your data lake / mounted
  bucket; the catalog keeps only the metadata + search projection. The
  authoritative bytes stay outside PostgreSQL.
- **`store_source = true`** — the sync stores each concept's exact bytes in
  `pgokf.concept_source` (retrievable with `pgokf.get_concept_source` /
  `pgokf.export_sources`), for a small, self-contained install.

`store_source` is read at sync time and is **not retroactive** — set it before
the first `register_bundle`. See the
[storage-tiers section of configuration](configuration.md).

---

## Author, register, query — the loop

1. **Copy a template** into a bundle directory and fill it in:

   ```bash
   mkdir -p /abs/path/to/my-bundle
   cp templates/service.md /abs/path/to/my-bundle/payments-api.md
   ```

2. **Register** the bundle (requires `pgokf_writer`):

   ```sql
   SELECT * FROM pgokf.register_bundle('/abs/path/to/my-bundle', 'payments');
   ```

3. **Refresh** after edits (incremental — only changed files are re-parsed):

   ```sql
   SELECT * FROM pgokf.refresh_bundle(<bundle_id>);
   ```

4. **Query** it (requires `pgokf_reader`) — see the
   [search guide](search-guide.md):

   ```sql
   SELECT * FROM pgokf.concept_search('payments restart');
   SELECT * FROM pgokf.concept_neighbors('service', 2);
   ```

---

## See also

- [Templates](https://github.com/LogicOcean/pgokf/blob/main/templates/) — copy-and-fill starters for every field on this
  page.
- [Search guide](search-guide.md) — querying the catalog you just authored.
- [SQL API reference](sql-api.md) — every function, table column, and SQLSTATE.
- [Configuration](configuration.md) — `store_source`, `default_strict`,
  `default_exclude`, `default_text_search_config`, and the GUC ceilings.
- [OKF v0.2 spec](https://github.com/GoogleCloudPlatform/knowledge-catalog) —
  the `okf/SPEC.md` document.
