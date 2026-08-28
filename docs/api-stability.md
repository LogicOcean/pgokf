# API Stability Policy

This document defines the **public API contract** for the `pgokf` extension:
what callers may depend on, how it is versioned, how it changes, and what is
explicitly *not* part of the contract. It is the reference that
[CHANGELOG.md](https://github.com/LogicOcean/pgokf/blob/main/CHANGELOG.md) classifies changes against and that the
[release checklist](release-checklist.md) enforces.

## What "public API" means here

The public API is the surface an application, dashboard, or migration can bind
to and expect to keep working across compatible releases. For `pgokf` that
surface is deliberately narrow and fully enumerated below. Everything reachable
in the database that is *not* on these lists is internal and may change without
notice.

Every public object is self-describing: it carries a SQL `COMMENT ON`, so
`\df+`, `\dT+`, `\d+`, and `obj_description()` document the contract from inside
the database. Complete comment coverage is a release gate (see
[Documentation coverage](#documentation-coverage-is-part-of-the-contract)).

## The stable surface

### Functions (21)

| Function | Role required | Purpose |
| -------- | ------------- | ------- |
| `pgokf.register_bundle(text, text, jsonb)` | `pgokf_writer` | Register and sync an OKF bundle root from a filesystem path |
| `pgokf.register_bundle_content(text, text[], bytea[], jsonb)` | `pgokf_writer` | Register or resync a bundle from in-memory `(path, bytes)` content — the mountless object-store ingestion path (no filesystem, no network I/O in the extension) |
| `pgokf.refresh_bundle(bigint)` | `pgokf_writer` | Incrementally re-sync a filesystem-sourced bundle (content bundles are re-synced via `register_bundle_content`) |
| `pgokf.unregister_bundle(bigint)` | `pgokf_writer` | Remove a bundle (rows cascade) |
| `pgokf.set_bundle_enabled(bigint, boolean)` | `pgokf_writer` | Enable/disable a bundle (hides it from search and graph without deleting rows; reversible) |
| `pgokf.list_bundles()` | `pgokf_reader` | List registered bundles |
| `pgokf.bundle_info(bigint)` | `pgokf_reader` | Info for one registered bundle |
| `pgokf.concept_search(text, bigint, integer)` | `pgokf_reader` | Ranked full-text search |
| `pgokf.concept_neighbors(text, integer, bigint)` | `pgokf_reader` | Walk the resolved link graph |
| `pgokf.set_config(text, jsonb)` | `pgokf_admin` | Set a durable configuration key |
| `pgokf.reset_config(text)` | `pgokf_admin` | Reset one/all configuration keys |
| `pgokf.get_config()` | `pgokf_reader` | Effective configuration as jsonb |
| `pgokf.list_sync_log(bigint, integer)` | `pgokf_reader` | Sync/audit history rows (from the admin-only `pgokf_private.sync_log`) |
| `pgokf.catalog_stats()` | `pgokf_reader` | Per-bundle counts, sync recency, and staleness for operators |
| `pgokf.health()` | `pgokf_reader` | Liveness/readiness document as jsonb (counts, backend, `in_recovery`) |
| `pgokf.stale_concepts(bigint, timestamptz)` | `pgokf_reader` | Concepts past their OKF `stale_after` |
| `pgokf.get_concept_source(bigint, text)` | `pgokf_reader` | Return one concept's stored source bytes as `bytea` (no filesystem write) |
| `pgokf.export_parquet(bigint, text)` | `pgokf_admin` | Export a bundle projection to Parquet files |
| `pgokf.export_sources(bigint, text)` | `pgokf_admin` | Reconstruct a bundle's stored source files on disk, hash-verified |
| `pgokf.rebuild_search_index()` | `pgokf_admin` | (Re)build the optional pg_search BM25 index; a no-op with a notice when pg_search is not installed |
| `pgokf.version()` | `pgokf_reader` | Loaded shared-library version |

The function **name, schema, argument types, argument order, and result shape**
are all part of the contract. Default values that let callers omit trailing
arguments (`name`/`options` on `register_bundle`, `bundle_id`/`limit` on search
and neighbors) are contractual too: an existing call that omits them keeps
working.

### Composite types (8)

`pgokf.bundle_sync_result`, `pgokf.concept_search_result`,
`pgokf.concept_neighbor`, `pgokf.bundle_info`, `pgokf.export_result`,
`pgokf.sync_log_entry`, `pgokf.catalog_stat`, `pgokf.stale_concept`.

The set of columns, their names, and their types are stable. New columns are
**not** added to an existing composite type in a compatible release, because
`SELECT *` and positional row expansion would break; a new field ships as a new
type or a new function instead.

### Tables (8 public + 2 documented-internal)

Public, `SELECT`-able by `pgokf_reader`: `pgokf.bundles`, `pgokf.concepts`,
`pgokf.concept_metadata`, `pgokf.links`, `pgokf.concept_provenance`,
`pgokf.concept_verification`, `pgokf.concept_provenance_source`,
`pgokf.concept_source`.

These are a **read projection**. Callers may `SELECT` from them and depend on
existing column names and types; the columns listed in
[docs/sql-api.md](sql-api.md) are the contract. Direct `INSERT`/`UPDATE`/
`DELETE` is not part of the API — mutation goes exclusively through the
`SECURITY DEFINER` sync functions, and the tables carry no write grant to
`pgokf_reader`. New columns *may* be added to these projection tables in a
compatible release; code that pins to named columns (never `SELECT *` into a
fixed row type) is forward-compatible.

`pgokf_private.config` and `pgokf_private.sync_log` are listed here only because
they are catalog tables the documentation gate covers (the eight public tables
plus these two private ones). They are **internal state, not API** — read the
sync history through `pgokf.list_sync_log` and configuration through
`pgokf.get_config`; see [The private surface](#the-private-surface-not-api).

### Roles (3)

`pgokf_reader` < `pgokf_writer` < `pgokf_admin`. Their **names** and the
**privilege boundaries** between them are stable: readers may search and read
configuration; writers may additionally register/refresh/unregister bundles
(the intended tier for an ingestion pipeline); admins may additionally manage
configuration and run the file-writing exports. Each tier inherits the one
below (`pgokf_admin` → `pgokf_writer` → `pgokf_reader`). These are cluster-wide
roles and survive `DROP EXTENSION`.

### GUC names (5)

`pgokf.max_file_bytes`, `pgokf.max_bundle_files`, `pgokf.max_frontmatter_bytes`,
`pgokf.max_graph_hops`, `pgokf.log_level`. The **names** and their meaning are
stable; default values are tuning knobs and may be adjusted in a minor release
when a change is safe and documented.

## Semantic versioning policy

`pgokf` follows [Semantic Versioning 2.0.0](https://semver.org). Given
`MAJOR.MINOR.PATCH`, and treating the stable surface above as the contract:

- **MAJOR** — a breaking change to the stable surface: removing or renaming a
  function/type/table/role/GUC, changing a function's argument types or order,
  removing a default that callers relied on, changing a result-type column set,
  narrowing a privilege in a way that breaks existing callers, or changing
  documented behavior in an incompatible way.
- **MINOR** — backward-compatible additions: a new function, a new composite
  type, a new projection column, a new GUC, a new optional trailing argument
  with a default, or new forward-compatible behavior.
- **PATCH** — backward-compatible fixes: bug fixes, performance improvements,
  documentation, and internal refactors that leave the stable surface
  unchanged.

### Pre-1.0 caveat

**While the version is `0.x`, the surface is not yet frozen.** Per SemVer,
`0.y.z` makes no compatibility promise across `y` bumps. In practice this
project already treats the enumerated surface as stable and documents every
change in the changelog, but until a `1.0.0` tag is cut a `0.MINOR` bump *may*
carry a breaking change when it is called out explicitly in the changelog under
a `Changed` or `Removed` heading. Reaching `1.0.0` is the point at which the
MAJOR/MINOR/PATCH rules above become binding guarantees, and it is a deliberate
human release decision — not an automated version bump.

## Deprecation process

Nothing on the stable surface is removed abruptly. The process is:

1. **Announce.** Mark the object deprecated in the changelog under
   `Deprecated`, and update its SQL `COMMENT ON` to say so and to name the
   replacement.
2. **Coexist.** The deprecated object keeps working for at least one full
   MINOR release (post-1.0) alongside its replacement, so callers can migrate
   incrementally.
3. **Remove.** Removal happens only in a subsequent MAJOR release, listed under
   `Removed` in the changelog, with the migration path repeated there.

Extension upgrade scripts (`pgokf--<from>--<to>.sql`) carry any data migration
a deprecation requires and must remain forward-compatible — see the
[upgrade mechanism](release-checklist.md) — so
`ALTER EXTENSION pgokf UPDATE` never loses catalog data.

## The private surface (NOT API)

The following are internal implementation detail. They may change, move, or
disappear in any release, and callers must not depend on them:

- **The entire `pgokf_private` schema**, including `pgokf_private.config`. It is
  administrator-only state managed exclusively through `pgokf.set_config` /
  `pgokf.reset_config` / `pgokf.get_config`. Read and write it only through
  those functions.
- **Indexes, constraints, and trigger internals** on the projection tables.
  Their existence and names are not contractual; query behavior is.
- **The `SECURITY DEFINER` wiring, `search_path` pinning, and grant details**
  beyond the documented reader/writer/admin boundaries.
- **Rust module layout, the sync engine, and the parser crates.** Only the SQL
  surface is public.
- **The exact text of `COMMENT ON` strings and error message wording.** SQLSTATE
  codes raised by public functions (e.g. `23505` on duplicate registration,
  `22023` on invalid configuration, unknown-bundle errors) *are* documented
  behavior; the human-readable message text is not.

## Documentation coverage is part of the contract

Every public object must carry a `COMMENT ON`. This is enforced two ways:

- **At build time** — `crates/extension/tests/api_stability.rs` reads the
  extension SQL source and fails `cargo test` if any enumerated function, type,
  table, or role lacks a `COMMENT ON`, and if the count of public functions
  drifts from the locked surface.
- **At release time** — the [release checklist](release-checklist.md) runs an
  `obj_description` / `shobj_description` coverage query against a freshly
  installed extension and blocks the release if any public object is
  uncommented.

Adding a public object therefore requires, in the same change: the object, its
`COMMENT ON`, an entry in the enumerated contract test, a changelog entry, and
(if it changes stored data) an upgrade script.
