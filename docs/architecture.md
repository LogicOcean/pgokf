# `pgokf` architecture

## Purpose and scope

`pgokf` is a PostgreSQL extension that imports Open Knowledge Format (OKF) bundles—UTF-8 Markdown concept documents with YAML frontmatter—into a queryable catalog. The extension name and SQL schema are both `pgokf`; Debian packages follow the PostgreSQL convention, for example `postgresql-16-pgokf`.

Phase 1 provides ingestion, synchronization, metadata queries, native full-text search, and a stable foundation for the OKF v0.2 link graph. It does not replace the bundle on disk: Markdown remains the portable source of truth, while PostgreSQL is a transactional projection optimized for discovery.

## System context

```text
OKF bundle directory
  (*.md + YAML frontmatter)
          |
          v
  path validation / reader
          |
          v
  Markdown + YAML parser -----> diagnostics
          |
          v
  normalized concept records
          |
          +----> pgokf.bundles
          +----> pgokf.concepts ----> weighted tsvector / GIN
          +----> pgokf.links    ----> recursive graph queries
          |
          v
  transactional diff/upsert/delete
          |
          v
SQL API: pgokf.register_bundle(), pgokf.concept_search(), catalog views
```

## Components

### Extension boundary

The Rust/pgrx extension exposes SQL objects under the non-relocatable `pgokf` schema. `CREATE EXTENSION pgokf;` installs functions, tables/views, indexes, and migration-managed metadata. Public entry points are schema-qualified in documentation and examples.

### Bundle reader and security boundary

`pgokf.register_bundle(path)` treats the server-side filesystem as privileged input. The reader canonicalizes the requested root, enforces configured allowlisted roots, rejects traversal and symlink escapes, applies file/count/byte limits, and reads only accepted Markdown files. Registration is restricted to authorized roles; ordinary catalog/search reads can be granted separately.

The database never executes content from a bundle. Markdown, YAML scalar values, links, computation fences, and referenced resources are data only.

### Parser and normalization

For each non-reserved `.md` file, the parser:

1. computes the OKF concept ID from its bundle-relative path without `.md`;
2. decodes UTF-8 and splits the delimited YAML frontmatter from the Markdown body;
3. requires `type` and preserves recommended and v0.2 metadata (`title`, `description`, `resource`, `tags`, provenance, trust, lifecycle, and producer extensions);
4. extracts Markdown links without interpreting surrounding prose;
5. returns structured diagnostics with path and category.

`index.md` and `log.md` are reserved OKF files and are not ordinary concepts. Unknown frontmatter keys are retained (for example in `metadata jsonb`) so round-tripping and future OKF versions do not lose producer data.

### Catalog projection

The logical model is:

- **`pgokf.bundles`** — one registered root: stable database ID, canonical path, sync state, timestamps, and aggregate digest.
- **`pgokf.concepts`** — one row per `(bundle_id, concept_id)`: relative path, type, title, description, resource, tags, body, structured metadata, content digest, timestamps, and native search vector.
- **`pgokf.links`** (v0.2 graph surface) — directed edges extracted from Markdown: source concept, raw destination, normalized target concept when internal/resolved, label, and resolution/external flags.
- **diagnostics/result rows** — parse and sync outcomes returned to the caller; malformed files do not become partially valid concept rows.

Exact physical columns may evolve behind public functions and views. Keys must make concepts with the same path in different bundles distinct.

### Transactional synchronization

Registration scans and parses before mutating catalog state. A successful sync applies a set-based diff in one transaction:

- insert new concepts;
- update changed concepts identified by content digest;
- preserve unchanged rows;
- remove catalog rows for files deleted from the registered bundle;
- replace/reconcile outgoing links for changed concepts;
- update bundle sync metadata last.

Fatal root/read/configuration errors abort the sync. Per-file malformed input is reported according to the selected strictness policy; strict mode must avoid committing a partial projection. Concurrent syncs of one bundle are serialized with a bundle-scoped lock.

### Search

The mandatory backend is PostgreSQL native FTS so every supported PostgreSQL 15–19 installation works without another extension. A weighted document favors title, then tags/type/description, then body, with a GIN index for matching and `ts_rank_cd` for ranking. Results add concept ID as a deterministic tiebreaker.

The public contract is `pgokf.concept_search(query)`. A future optional adapter can use ParadeDB `pg_search`/BM25 while returning the same logical result shape. ParadeDB must never be a transitive requirement of `CREATE EXTENSION pgokf`; see [BM25 research](bm25-research.md).

### Link graph (OKF v0.2)

Internal Markdown destinations are normalized relative to the source document or bundle root. Fragment identifiers do not change the target concept ID. External URLs are retained but do not become internal edges. Broken internal links are retained as unresolved because OKF explicitly permits them and later syncs may resolve them.

Recursive SQL traversals must be cycle-safe, bundle-scoped, depth-limited, and authorization-filtered. The examples in `examples/queries/graph.sql` show both outgoing edges and a cycle-safe recursive CTE.

## Data and API invariants

- A concept ID is bundle-relative and has no `.md` suffix.
- `(bundle_id, concept_id)` is unique.
- Source bytes must be valid UTF-8; normalization must not silently rewrite user text.
- The parser never trusts a producer-defined `id` over the path-derived OKF ID; duplicate producer `id` extensions are diagnostics/test inputs, not catalog keys.
- Sync and search obey PostgreSQL transaction visibility and role permissions.
- Backend-specific relevance scores are comparable only within one query/backend; callers should rely on ordering, not persist raw scores.
- SQL objects and examples use `pgokf`, never the former `okf_catalog`/`okf` names.

## Packaging and compatibility

The target matrix is PostgreSQL 15, 16, 17, 18, and 19. Rust feature flags and pgrx test initialization are version-specific; CI exercises each target. Distribution artifacts use names such as:

```text
postgresql-15-pgokf
postgresql-16-pgokf
postgresql-17-pgokf
postgresql-18-pgokf
postgresql-19-pgokf
```

The core package depends only on PostgreSQL/pgrx and its bundled Rust libraries. Optional search integrations are separately detected, documented, and tested.

## Observability and failure handling

Registration returns counts and diagnostics suitable for CI and operators. Server logs should include bundle identity and high-level failure categories without dumping full concept bodies. Metrics worth exposing later include scan duration, files/bytes read, inserted/updated/deleted/unchanged counts, parse failures, unresolved links, and search latency.

## Evolution

Schema migrations are extension upgrade scripts. Public SQL functions and stable views are the compatibility boundary; callers should not depend on private storage tables. Planned evolution includes richer graph APIs, incremental filesystem watching outside the backend process, relevance evaluation, and an optional BM25 adapter after compatibility and licensing review.
