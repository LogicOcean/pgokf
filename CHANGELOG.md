# Changelog

All notable changes to the `pgokf` PostgreSQL extension are documented in this
file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project aims to adhere to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The public API contract and what "a breaking change" means for this extension
are defined in [docs/api-stability.md](docs/api-stability.md).

## [Unreleased]

Nothing yet.

## [0.1.10] - 2026-08-28

**OKF-conformance batch**: an Attested Computation concept's type-specific
reference fields now become traversable graph edges, and the reserved per-
directory `log.md` activity log is now projected instead of dropped. Everything
is additive and backward compatible, so `ALTER EXTENSION pgokf UPDATE TO '0.1.10'`
migrates an existing install in a single transaction and yields a catalog
identical to a fresh 0.1.10. The one new `pgokf.links` column is backfilled by
its default.

### Added

- **Attested Computation reference fields as graph edges** — for a concept whose
  `type` is `Attested Computation`, its `computation`, `executor`, and `attester`
  reference fields (each a bare resource path or a `{resource: …}` mapping) are
  resolved into **`pgokf.links`** as typed internal edges, numbered after the
  concept's body links. `pgokf.concept_neighbors` now traverses them like any
  resolved internal edge, so the executor/attester/computation concepts are
  reachable even when the body links to none of them. A missing, external, or
  dangling reference is retained as `is_external` / `resolved = false` and never
  traversed, exactly like any other link. Non-attested concepts are unaffected.
- **`pgokf.links.link_relation`** (`text NOT NULL DEFAULT 'reference'`) — a new
  additive column carrying the edge's semantic relation, distinct from the
  Markdown-construct `link_kind`: `reference` for every ordinary link, or
  `attestation:computation` / `attestation:executor` / `attestation:attester`
  for the new typed edges. Existing rows are backfilled to `reference`.
- **Reserved `log.md` projection** — the per-directory OKF `log.md` activity
  logs, previously skipped entirely, are now parsed and projected into a new
  **`pgokf.bundle_log`** table (`bundle_id`, `tenant_id`, `directory`, `ordinal`,
  `logged_at`, `entry`; PK `(bundle_id, directory, ordinal)`; cascades from
  `pgokf.bundles`; opt-in multi-tenant RLS). Each non-blank line becomes one
  entry, with a leading ISO 8601 timestamp lifted into `logged_at` and the line
  stored losslessly. The projection is replaced wholesale on every sync, so it
  tracks edits/additions/removals; a `log.md` is still **never** a concept and
  never counts toward the bundle's `file_count`. `index.md` handling is
  unchanged.
- **`pgokf.list_bundle_log(bundle_id bigint, directory text DEFAULT NULL,
  max_rows int DEFAULT 500)`** (`SETOF pgokf.bundle_log_entry`, reader-level,
  `STABLE PARALLEL SAFE`, invoker rights) — lists a bundle's log entries ordered
  by directory then ordinal, optionally scoped to one directory (`''` for the
  root). New composite **`pgokf.bundle_log_entry(bundle_id, directory, ordinal,
  logged_at, entry)`**. Raises `22023` when `max_rows < 0`.

## [0.1.9] - 2026-08-28

**Search and scheduling batch**: keyset pagination and faceted counts on search,
a search-index coverage report, and an optional `pg_cron` scheduled re-sync.
Everything is additive and backward compatible, so
`ALTER EXTENSION pgokf UPDATE TO '0.1.9'` migrates an existing install in a single
transaction and yields a catalog identical to a fresh 0.1.9. `concept_search`
gains one optional trailing argument (documented below); no existing type or
default changes.

### Added

- **Keyset / cursor pagination on `concept_search`** — a new optional trailing
  argument **`after_cursor jsonb DEFAULT NULL`**. Ranked results now have a stable
  total order (`rank DESC`, then `bundle_id ASC`, then `concept_id ASC`); copy the
  `rank`, `bundle_id`, and `concept_id` of a page's last row into `after_cursor`
  and the next page continues strictly after it, with **no `OFFSET` drift and no
  duplicates or skips even when ranks tie**. Applied in both the native and BM25
  backends. A malformed cursor raises `22023`. The historical three- through
  seven-argument calls are unchanged (`after_cursor` defaults to the first page).
- **Faceted result counts** — **`pgokf.search_facets(query, bundle_id DEFAULT
  NULL, facet DEFAULT 'type', concept_type DEFAULT NULL, tags DEFAULT NULL, status
  DEFAULT NULL, trust_tier DEFAULT NULL)`** (`SETOF pgokf.search_facet`,
  reader-level) counts the same matching set `concept_search` would, grouped by
  one facet — `type`, `bundle`, `status`, `trust_tier`, or `tag` (any other value
  raises `22023`; the facet is dispatched on, never interpolated). The `tag` facet
  counts a concept once per tag. New composite **`pgokf.search_facet(facet_value
  text, count bigint)`**.
- **Search-index health / coverage** — **`pgokf.search_index_status()`**
  (`jsonb`, reader-level) reports the configured backend, that native FTS is
  always available, and for each optional index whether its extension is
  installed, whether the index exists, and how much of the catalog it covers
  (BM25 rows and embedding-vector coverage vs. total concepts). Coverage counts
  are tenant-scoped.
- **Optional `pg_cron` scheduled re-sync** — **`pgokf.schedule_refresh(bundle_id,
  schedule)`** (`text`, admin-tier) registers an idempotent
  `pgokf_refresh_<bundle_id>` cron job running `SELECT pgokf.refresh_bundle(<id>)`
  on the given schedule, and **`pgokf.unschedule_refresh(bundle_id)`** (`boolean`,
  admin-tier) removes it. The coupling to `pg_cron` is runtime-only (mirroring the
  pgvector / `pg_search` optional-dependency seam): `CREATE EXTENSION pgokf`
  succeeds without `pg_cron`, and when it is absent `schedule_refresh` raises a
  clear `22023` naming the missing dependency while `unschedule_refresh` is a
  clean no-op. Full scheduling requires `pg_cron` in `shared_preload_libraries`.

### Changed

- **`concept_search` result order is now a stable total order** (`rank DESC,
  bundle_id ASC, concept_id ASC`), replacing the previous `rank DESC, concept_id
  ASC`. This only refines the tiebreak for equal-rank hits and is what makes
  keyset pagination exact.
- The `0.1.8 → 0.1.9` upgrade replaces the seven-argument `concept_search`
  overload with the eight-argument superset (`DROP` old + `CREATE` new, in one
  transaction), exactly as `0.1.5 → 0.1.6` did, so an upgraded catalog carries a
  single `concept_search` overload identical to a fresh install.

## [0.1.8] - 2026-08-28

**Lifecycle and audit batch**: a per-sync change manifest, a reversible bundle
retirement window, an exfiltration/access audit, and cross-bundle content
deduplication. Everything is additive and backward compatible, so
`ALTER EXTENSION pgokf UPDATE TO '0.1.8'` migrates an existing install in a single
transaction and yields a catalog identical to a fresh 0.1.8. Six new public
functions are added; no existing signature, type, or default changes.

### Added

- **Per-concept change manifest** — every `register` / `refresh` / `content`
  sync now records which concepts it added, updated, or removed, not just the
  aggregate counts. Stored in the new administrator-only
  `pgokf_private.sync_log_change` (a child of `pgokf_private.sync_log`, cascading
  on delete so it shares the `sync_log_retention_days` window) and read through
  the reader-level **`pgokf.list_sync_changes(sync_id, max_rows DEFAULT 1000)`**
  (`SETOF pgokf.sync_change`), tenant-scoped like `list_sync_log`.
- **Bundle retirement / soft-delete window** — a new `bundles.retired_at`
  timestamp and three functions: **`pgokf.retire_bundle(bundle_id)`** and
  **`pgokf.unretire_bundle(bundle_id)`** (writer-tier), and
  **`pgokf.purge_retired(older_than interval DEFAULT '7 days')`** (admin-tier).
  A bundle is *active* only when `enabled AND retired_at IS NULL`; a retired
  bundle is excluded from `concept_search`, `concept_neighbors`, semantic/hybrid
  search, and the default `list_bundles` without deleting any rows, so retirement
  is a reversible undo window for the hard `unregister_bundle` cascade.
  `purge_retired` hard-deletes bundles retired longer than the interval (writing
  one `unregister` audit row each). Retirement is idempotent (re-retiring keeps
  the original instant) and does not touch `enabled`.
- **Exfiltration / access audit** — the three content-exporting operations
  (`export_parquet`, `export_sources`, `get_concept_source`) now each append one
  row to the new administrator-only `pgokf_private.access_log` (who read/exported
  what, and when), read through the admin-tier
  **`pgokf.list_access_log(bundle_id DEFAULT NULL, max_rows DEFAULT 100)`**
  (`SETOF pgokf.access_log_entry`). The log shares the `sync_log_retention_days`
  retention window.
- **Cross-bundle content deduplication** —
  **`pgokf.duplicate_concepts(bundle_id DEFAULT NULL, min_group int DEFAULT 2)`**
  (`SETOF pgokf.duplicate_group`, reader-level) groups byte-identical concepts by
  their stored BLAKE3 `file_hash`, so an operator can find the same runbook or
  reference copied across bundles.
- **`retired_at`** on the `pgokf.catalog_stat` composite (returned by
  `catalog_stats`), so retired bundles — hidden from `list_bundles` — stay
  visible with their retirement instant.

### Changed

- **`pgokf.get_concept_source`** is now `SECURITY DEFINER` and tenant-scoped (so
  it can append its access-audit row); its reader-tier grant and signature are
  unchanged.
- **`pgokf.list_bundles`** now excludes retired bundles by default (retired
  bundles remain reachable by id via `bundle_info` and visible in
  `catalog_stats`); disabled-but-not-retired bundles are still listed.
- The `sync_log_retention_days` policy now also governs `pgokf_private.access_log`
  and, transitively, the change manifest (via the `sync_log_change` cascade).

## [0.1.7] - 2026-08-28

**Opt-in multi-tenant isolation**, built from a per-session GUC and PostgreSQL
row-level security. Everything is strictly backward compatible: an existing
install, and any session that never sets a tenant, sees all rows and behaves
exactly as under 0.1.6, so `ALTER EXTENSION pgokf UPDATE TO '0.1.7'` migrates an
existing install in a single transaction and yields a catalog identical to a
fresh 0.1.7 (every existing row backfills to the `default` tenant). No public API
surface changes — no new functions, types, or arguments.

### Added

- **Denormalized `tenant_id`** (`text NOT NULL DEFAULT 'default'`) on every
  projection table — `bundles`, `concepts`, `concept_metadata`, `links`,
  `concept_provenance`, `concept_verification`, `concept_provenance_source`,
  `concept_source`, `concept_embedding` — and on `pgokf_private.sync_log`.
  Indexed where it helps (a dedicated index on `concepts`; on `bundles` the new
  `UNIQUE (tenant_id, path)` index already leads with it).
- **`pgokf.tenant` GUC** (`USERSET`, empty default) — the per-session tenant
  selector. Set it per session (`SET pgokf.tenant = 'acme'`), per login role
  (`ALTER ROLE r SET pgokf.tenant = ...`), or as a connection option; empty (the
  default) means the session declares no tenant and sees every row.
- **Row-level security on every projection table** with an opt-in-by-usage
  policy: a session that has not set `pgokf.tenant` matches all rows (backward
  compatible), a session that has set it matches only that tenant. RLS is enabled
  but *not forced*, so the `SECURITY DEFINER` write/admin functions bypass it to
  stamp and read within one single-tenant bundle.
- **`docs/multi-tenancy.md`** documenting the model, the per-tenant bundle keys,
  the `SECURITY DEFINER`-bypass reasoning, and the strict-isolation contract.

### Changed

- **Per-tenant bundle registration key.** `pgokf.bundles` is now keyed
  `UNIQUE (tenant_id, path)` instead of `UNIQUE (path)`, so two tenants may
  register the same filesystem or `content:<name>` path as independent bundles.
  The duplicate-registration `23505` check is scoped to the current tenant. (The
  upgrade replaces the old single-column key with this strict superset; no data
  is touched.)
- **Writes stamp the tenant.** `register_bundle` / `register_bundle_content`
  stamp the bundle row from `effective_tenant()`; every projected child row and
  the `set_concept_embedding` row inherit the bundle's tenant; the `sync_log`
  row records the operating tenant. `refresh_bundle`, `unregister_bundle`, and
  `set_bundle_enabled` operate on an existing bundle and never change its tenant.
- **`list_sync_log` and `health` are tenant-scoped.** Both are `SECURITY DEFINER`
  (they bypass RLS), so they apply the same opt-in tenant filter explicitly:
  `list_sync_log` filters its rows and `health`'s `bundle_count` / `concept_count`
  are scoped, each a no-op for an unset session.

## [0.1.6] - 2026-08-28

An additive **search-enhancement** batch: structured filters on ranked search, a
content more-like-this, and an optional pgvector semantic / hybrid surface.
Everything is backward compatible — the historical `concept_search(query,
bundle_id, limit_count)` call is unchanged — so `ALTER EXTENSION pgokf UPDATE TO
'0.1.6'` migrates an existing install in a single transaction and yields a
catalog byte-identical to a fresh `0.1.6` (verified by diffing the two).

### Added

- **Structured filters on `pgokf.concept_search`.** Four optional trailing
  arguments, each a no-op when `NULL`: `concept_type text`, `tags text[]`
  (**ALL-of** containment — a hit must carry every listed tag), `status text`,
  and `trust_tier text` (matched against `pgokf.concept_provenance`). The filters
  are parameter-bound `AND` clauses applied in both the native and BM25 backends,
  reusing the existing `tags`, `type`, and provenance indexes.
- **`pgokf.find_similar(concept_id text, bundle_id bigint DEFAULT NULL,
  limit_count int DEFAULT 10)`** — content more-like-this. It extracts a seed
  concept's most salient `body_tsv` lexemes and ranks other concepts against them
  through the configured `search_backend` (native FTS or BM25), excluding the
  seed. Distinct from `concept_neighbors` (the authored link graph).
- **Optional semantic + hybrid search via pgvector** (mirroring the optional
  BM25 seam exactly — `CREATE EXTENSION pgokf` still succeeds without pgvector):
  - **`pgokf.concept_embedding`** stores per-concept vectors as the builtin
    `real[]` (never a `vector` column, so the extension takes no static pgvector
    dependency), cast to `vector(embedding_dim)` only at query and index time.
  - **`pgokf.set_concept_embedding(bundle_id, concept_id, embedding real[])`**
    (writer-tier) is how a companion embedder streams caller-computed vectors in;
    the extension never computes embeddings or performs network I/O.
  - **`pgokf.concept_search_semantic(query_embedding real[], …)`** ranks by
    pgvector cosine distance; the `rank` column is the normalized cosine
    similarity. It **requires pgvector** and raises `22023` naming the missing
    dependency when it is absent (semantic search has no lexical fallback).
  - **`pgokf.concept_search_hybrid(query text, query_embedding real[], …)`** fuses
    the lexical and semantic results with **Reciprocal Rank Fusion** (RRF,
    k = 60) entirely in SQL. When pgvector is absent it degrades to lexical-only
    with a `WARNING`.
  - **`pgokf.rebuild_embedding_index()`** (admin-tier, mirroring
    `rebuild_search_index`) builds a pgvector HNSW cosine index for the configured
    dimension; a logged no-op when pgvector is absent or the dimension exceeds
    pgvector's 2000-dim HNSW limit.
  - New config key **`embedding_dim`** (integer, default 1536) governs the
    expected embedding length and the HNSW index typmod.

### Changed

- `pgokf.concept_search` gained the four trailing filter arguments (a new
  function *identity* in `pg_proc`). The upgrade script removes the superseded
  three-argument overload and creates the seven-argument one, so an upgraded
  catalog carries exactly one `concept_search` overload — identical to a fresh
  install — and every historical one-, two-, and three-argument call still
  resolves through the new defaults.

## [0.1.5] - 2026-08-28

An additive **audit, lifecycle, and observability** batch. Everything is
backward compatible — a new admin-only table, three composite types, five new
functions, three new configuration keys, and two functions whose *behavior*
gained a filter — so `ALTER EXTENSION pgokf UPDATE TO '0.1.5'` migrates an
existing install in a single transaction (the `0.1.4 → 0.1.5` upgrade script
adds the `sync_log` table, the three types, the five functions, and the two
config columns; the rest lives in the shared library and activates on load).

### Added

- **Sync/audit log.** A new administrator-only `pgokf_private.sync_log` records
  one row per successful `register` / `refresh` / `register_bundle_content` sync
  and per `unregister`, inside the operation's own transaction (so a logged row
  always means the operation committed). Read it with the reader-level
  **`pgokf.list_sync_log(bundle_id, max_rows)`** (returning the new
  `pgokf.sync_log_entry`). This also **activates the previously dead
  `sync_log_retention_days` key**: after each append, history older than the
  window is pruned in the same transaction (`0` keeps it indefinitely).
- **Bundle enable/disable lifecycle.** **`pgokf.set_bundle_enabled(bundle_id,
  enabled)`** (writer-tier) hides a bundle from ranked search *and* graph
  traversal without deleting any rows, and is fully reversible.
- **`concept_neighbors` now excludes disabled bundles**, matching
  `concept_search`, so a disabled bundle's concepts are neither returned nor
  traversed.
- **Change notification.** A new `notify_channel` configuration key: when set to
  a safe channel identifier, a successful sync emits
  `pg_notify(<channel>, {bundle_id, op, added, updated, removed, total})`.
  Off by default (empty) with zero overhead.
- **Observability functions** (all reader-level): **`pgokf.catalog_stats()`**
  (per-bundle indexed-concept / link / resolved-link counts, sync recency, and a
  24-hour staleness flag → `pgokf.catalog_stat`), **`pgokf.health()`** (a
  `jsonb` liveness/readiness document: `ok`, counts, `search_backend`,
  `bm25_ready`, `in_recovery`, `roles_ok`, `config_ok`), and
  **`pgokf.stale_concepts(bundle_id, as_of)`** (concepts past their OKF
  `stale_after` → `pgokf.stale_concept`).
- **OKF version conformance.** A new `okf_version_policy` key (`warn` | `reject`,
  default `warn`): a bundle declaring an OKF `okf_version` this build does not
  support (only `0.2` / `0.2.x`) is warned about and indexed under `warn`, or
  rejected with `22023` under `reject`. An absent `okf_version` is unaffected.
  The `okf-parser` crate gains a small, centralized `is_supported_okf_version`.

### Changed

- `sync_log_retention_days` moves from **defined-but-dead** to **active** (see
  above). `notify_channel` and `okf_version_policy` are new, active keys.
- **Internal:** a behavior-preserving complexity refactor of the parser,
  config-coercion, and SPI-row-reading hot paths — a shared `spi_read` tuple
  helper (DRY), per-key config coercion/defaults, and decomposed ISO-8601
  parsers — dropping the worst function's cyclomatic complexity from 39 to 18
  with no change to any behavior, signature, SQL surface, or test.

## [0.1.4] - 2026-08-28

Two additive capabilities landed together: a **`pgokf_writer` ingestion role
tier** paired with an **optional BM25 search backend**, and a **mountless
object-store ingestion path** (`register_bundle_content` plus the standalone
`pgokf-ingest` companion). Everything here is backward compatible — new
functions, a new role, a new projection column, and a new configuration key —
so `ALTER EXTENSION pgokf UPDATE TO '0.1.4'` migrates an existing install in a
single transaction (the `0.1.3 → 0.1.4` upgrade script creates the writer role,
adds `rebuild_search_index`, and adds `register_bundle_content` +
`bundles.source_type`).

### Added

- **`pgokf_writer` role — a new ingestion tier** between `pgokf_reader` and
  `pgokf_admin` (`pgokf_reader` < `pgokf_writer` < `pgokf_admin`, each inheriting
  the tier below). It is the intended account for an automated ingestion
  pipeline: it can register/refresh/unregister bundles but cannot change
  configuration, write exports, or read `pgokf_private`.
- **`pgokf.register_bundle_content(name text, paths text[], contents bytea[], options jsonb)`**
  — the *mountless* ingestion path. A companion process reads an object store and
  streams the collected `(path, bytes)` into PostgreSQL; the extension itself
  performs no network or filesystem I/O. Re-calling it resyncs the bundle
  (changed concepts upserted, missing ones deleted) exactly like a filesystem
  refresh, with the same `max_bundle_files` / `max_file_bytes` bounds and
  `store_source` round-trip. Writer-tier, `SECURITY DEFINER`.
- **`pgokf.bundles.source_type`** (`'filesystem'` | `'content'`, default
  `'filesystem'`) distinguishing a bundle registered from a canonical on-disk
  root from one streamed in memory (keyed on the synthetic path `content:<name>`).
- **`pgokf.rebuild_search_index()`** — admin function that (re)builds the optional
  `pg_search` BM25 index; a no-op with a notice when `pg_search` is not installed.
- **`search_backend` configuration key** (`native` | `bm25`, default `native`).
  `native` uses the built-in `websearch_to_tsquery` / `ts_rank_cd` ranking;
  `bm25` routes `concept_search` through ParadeDB `pg_search` at runtime via SPI
  when available, falling back to native (with a warning) when it is not — so the
  extension takes no hard dependency on `pg_search`.
- **`pgokf-ingest` companion crate** — a standalone async binary that lists an
  S3-compatible object store (MinIO / SeaweedFS / AWS S3 / GCS / Azure via
  `object_store`), downloads the objects, and streams them to
  `register_bundle_content` as `pgokf_writer`. Object-store credentials live in
  the companion and never reach PostgreSQL. It is a separate workspace member and
  does not affect the extension build.

### Changed

- **Ingestion moved to the writer tier (backward compatible).**
  `pgokf.register_bundle`, `pgokf.refresh_bundle`, and `pgokf.unregister_bundle`
  now require `pgokf_writer` instead of `pgokf_admin`. Existing admin callers keep
  working because `pgokf_admin` inherits `pgokf_writer`; configuration and the
  file-writing exports remain admin-only.
- **`refresh_bundle` rejects content-sourced bundles.** A `source_type = 'content'`
  bundle has no filesystem root, so `refresh_bundle` raises `22023` for it —
  re-sync those by calling `register_bundle_content` again.
- **Internal:** the sync engine was refactored around a `ByteSource` seam so the
  filesystem path (walk + read) and the content path (caller-supplied bytes)
  share one classify → parse → upsert → project pipeline. Filesystem
  `register_bundle` / `refresh_bundle` behavior is unchanged.

## [0.1.3] - 2026-08-28

OKF v0.2 conformance re-model of the provenance / trust / lifecycle projection,
and population of `pgokf.bundles.okf_version`. This is a **breaking change** to
the `pgokf.concept_provenance` shape; because the extension is pre-release (no
tagged release, no external installs), the schema is changed in place with no
compatibility shim.

### Changed

- **`pgokf.concept_provenance` re-modeled to OKF v0.2 (breaking).** The invented,
  non-OKF columns `verified` (a flattened bool), `verification_method`, and
  `freshness` are removed. The table now carries the real OKF v0.2 fields:
  `generated_by` (`generated.by`), `generated_at` (`generated.at`), `status`
  (LIFECYCLE `status`), `stale_after`, `usage_window_from` / `usage_window_to`
  (top-level `usage_window`), and a `trust_tier` **derived** from the
  verification actors (`unverified` → `machine-confirmed` → `human-reviewed`).
  Timestamps are ISO 8601, parsed defensively (a malformed instant projects
  `NULL`, never aborting the sync); the recognized key subset is kept losslessly
  in `details`. The index on `verified` is replaced by an index on `trust_tier`.
- **`pgokf.bundles.okf_version` is now populated.** The sync engine reads the
  optional `okf_version` from the reserved bundle-root `index.md` frontmatter
  (string or number, e.g. `0.2`) and stores it; an absent or malformed value
  leaves the column `NULL`. It surfaces unchanged through `bundle_info` /
  `list_bundles`.

### Added

- **`pgokf.concept_verification` table** — the ordered OKF `verified[]` event
  list, one `(bundle_id, concept_id, ordinal)` row per `{by, at}` event (a single
  `verified` mapping is stored as one `ordinal = 0` row; actorless events are
  skipped). Cascades from `pgokf.concepts`; reader-`SELECT`able.
- **`pgokf.concept_provenance_source` table** — the OKF `sources[]` provenance
  materials, one row per entry (`source_id`, `resource`, `title`, `author`,
  `usage_count`, `last_modified`, per-source `usage_window_from` / `_to`).
  Distinct from the raw-bytes `pgokf.concept_source`. Cascades from
  `pgokf.concepts`; reader-`SELECT`able.

### Fixed

- **`export_parquet` epoch cast for OKF v0.2 provenance timestamps.** The
  re-modeled `pgokf.concept_provenance.generated_at` is a `timestamptz`;
  `export_parquet` now converts it to epoch microseconds
  (`(EXTRACT(EPOCH FROM generated_at) * 1000000)::bigint`) so the Parquet writer
  emits a portable `Timestamp(µs, UTC)` column. Verified round-trippable in
  DuckDB via an in-database test.

### Security

- **Closed an `export_sources` write-escape via a symlinked parent directory.**
  `export_sources` recreates a bundle's directory tree under `dest_dir`; a
  symlink planted at an intermediate path component could previously redirect a
  write outside the validated destination. Writes now use the same `O_NOFOLLOW`
  open as `export_parquet` on the final component and re-validate every stored
  concept path as a plain bundle-relative path, so a planted symlink is refused
  (`22023`) instead of followed. Each reconstructed file is additionally
  verified against its recorded BLAKE3 `file_hash` before creation
  (`XX000` on mismatch, nothing written).

### Upgrade

- No supported in-place upgrade from `0.1.2`: this pre-release drops and
  re-creates the provenance projection. Re-`CREATE EXTENSION` and re-register
  bundles; because the on-disk bundle is the source of truth, the projection is
  fully rebuilt from a sync.

## [0.1.2] - 2026-08-27

Additive, opt-in raw source storage. Default behavior is unchanged: the new
`store_source` policy is **off by default**, so an install that never enables it
is byte-for-byte identical to 0.1.1.

### Added

- **`store_source` configuration key** (boolean, default `false`) on
  `pgokf_private.config`. It selects between two deployment tiers: `true` stores
  each concept's verbatim source bytes in PostgreSQL (small, self-contained
  install — no external storage needed); `false` keeps the source in a mounted
  object store / data lake and PostgreSQL holds only metadata and search. Like
  `default_text_search_config`, it is read at sync time and is **not
  retroactive** — set it before the first `register_bundle`, or re-register.
- **`pgokf.concept_source` table** — opt-in verbatim source bytes
  (`raw_content bytea`, `byte_size integer`), keyed `(bundle_id, concept_id)` and
  cascading from `pgokf.concepts`, so removals and unregistration drop the stored
  source automatically. TOAST-compressed with `lz4` where the build supports it,
  otherwise `pglz`. Reader-`SELECT`able.
- **`pgokf.get_concept_source(bundle_id, concept_id) → bytea`** — reader-level
  retrieval of a concept's exact stored bytes to the client (no filesystem
  write). Raises `22023` when the concept exists but no source was stored, and,
  distinctly, when no such concept exists.
- **`pgokf.export_sources(bundle_id, dest_dir) → pgokf.export_result`** —
  admin-only reconstruction of a bundle's stored source files on disk,
  byte-for-byte. Reuses `export_parquet`'s destination validation and
  `O_NOFOLLOW` file creation, recreates the bundle-relative directory tree, and
  verifies each written file against the concept's BLAKE3 `file_hash`.

### Changed

- The sync engine now persists source bytes into `pgokf.concept_source` when
  `store_source` is enabled, projected inside the same atomic, advisory-locked
  transaction as links and provenance (no change when the key is off).

### Upgrade

- `ALTER EXTENSION pgokf UPDATE TO '0.1.2'` brings a 0.1.1 install fully to 0.1.2
  with no data loss: it adds the `store_source` column (default `false`), the
  `concept_source` table, and the two new functions, and touches no existing
  object.

## [0.1.1] - 2026-08-27

Hardening, performance, and packaging. No public-API change: the stable surface
(functions, types, tables, roles, GUCs) is byte-for-byte identical to 0.1.0, so
`ALTER EXTENSION pgokf UPDATE TO '0.1.1'` is a proven no-data-loss step.

### Fixed

- **A large concept body could abort an otherwise-valid sync.** The body
  `tsvector` is now fully bounded so no document within the configured size
  limits can raise PostgreSQL's `tsvector` size error mid-sync; the whole
  transaction no longer rolls back on a single large-but-in-limit file.
- **Resolved the findings from a full-repository adversarial audit** across the
  parser, sync engine, and catalog surface — input-validation edges, error
  mapping, and path-handling corners hardened without changing behavior for
  well-formed input.

### Performance

- **Batched SPI inserts in the sync engine** — concepts, metadata, links, and
  provenance are projected in batched statements instead of row-at-a-time,
  cutting per-file round trips on large bundles.
- **Guarded the link re-resolution `UPDATE`** so an incremental
  `refresh_bundle` only re-resolves links whose target set actually changed,
  avoiding needless writes on unchanged concepts.

### Added

- **Distribution packaging** — `.deb` / `.rpm` build recipes, a PGXN
  `META.json`, a Docker image, and a Homebrew formula, wired into a `packages`
  CI job so per-major artifacts build reproducibly.
- **Proven extension upgrade path.** The example `sql/pgokf--0.1.0--0.1.1.sql`
  upgrade script exercises `ALTER EXTENSION pgokf UPDATE TO '0.1.1'` end to end
  as a deliberate no-op, demonstrating the forward-compatible,
  never-`DROP`/`TRUNCATE`/`DELETE` migration contract that
  `tests/api_stability.rs` enforces on every shipped script.

## [0.1.0] - 2026-08-27

The first tagged release: a complete, transactional PostgreSQL catalog for
Open Knowledge Format (OKF) bundles. The bundle on disk stays the portable
source of truth; PostgreSQL becomes a projection optimized for metadata
queries, native full-text search, and link-graph traversal.

### Added

- **Bundle registration and sync.** `pgokf.register_bundle(path, name, options)`
  ingests an OKF bundle root and `pgokf.refresh_bundle(bundle_id)` incrementally
  re-synchronizes it, re-parsing only files whose BLAKE3 content hash changed
  and removing rows for deleted files. `pgokf.unregister_bundle(bundle_id)`
  removes a bundle; concepts, metadata, links, and provenance cascade.
- **Catalog projection.** Base tables `pgokf.bundles`, `pgokf.concepts`, and
  `pgokf.concept_metadata`, plus the feature projections `pgokf.links`
  (concept-to-concept link graph) and `pgokf.concept_provenance` (generation
  and verification lineage).
- **Full-text search.** `pgokf.concept_search(query, bundle_id, limit)` returns
  ranked hits with `ts_headline` snippets over a weighted `tsvector` (title A,
  tags/type/description B, body D). Native PostgreSQL FTS only — no third-party
  search extension is required.
- **Link-graph traversal.** `pgokf.concept_neighbors(concept_id, max_hops,
  bundle_id)` walks the resolved link graph outward from a concept.
- **Administration.** `pgokf.list_bundles()` and `pgokf.bundle_info(bundle_id)`
  expose the registered-bundle inventory as the `pgokf.bundle_info` type.
- **Durable configuration.** `pgokf.set_config`, `pgokf.reset_config`, and
  `pgokf.get_config` manage a single, typed, cluster-persistent policy row
  (`allowed_roots`, `default_text_search_config`, `default_strict`,
  `sync_log_retention_days`, `default_exclude`) stored in the
  administrator-only `pgokf_private.config` table.
- **Parquet export.** `pgokf.export_parquet(bundle_id, dest_dir)` writes a
  bundle's catalog projection to four Parquet files — `concepts.parquet`,
  `concept_metadata.parquet`, `links.parquet`, and `concept_provenance.parquet`
  — inside `dest_dir` for downstream analytics.
- **Version introspection.** `pgokf.version()` reports the loaded shared
  library's version for post-upgrade agreement checks.
- **Composite result types.** `pgokf.bundle_sync_result`,
  `pgokf.concept_search_result`, `pgokf.concept_neighbor`, `pgokf.bundle_info`,
  and `pgokf.export_result`.
- **Security model.** Two cluster roles, `pgokf_reader` (search and read
  configuration) and `pgokf_admin` (register/refresh/unregister and manage
  configuration, inherits `pgokf_reader`). Every mutating function is
  `SECURITY DEFINER` with a pinned `search_path`, `EXECUTE` is revoked from
  `PUBLIC` and granted only to the appropriate role, and bundle paths are
  validated (absolute, traversal-free, canonicalized, optionally confined to
  configured `allowed_roots`) before the server reads any file. The private
  `pgokf_private` schema is internal state, not API.
- **Configurable safety limits (GUCs).** `pgokf.max_file_bytes`,
  `pgokf.max_bundle_files`, `pgokf.max_frontmatter_bytes`,
  `pgokf.max_graph_hops`, and `pgokf.log_level`.
- **Documentation coverage.** Every public object — all 12 functions, all 5
  composite types, all 6 catalog tables, and both API roles — carries a
  `COMMENT ON`, enforced by the `api_stability` test suite and by a runtime
  `obj_description` coverage gate in the release checklist.
- **PostgreSQL 15–19 support**, built with Rust (edition 2024) and pgrx 0.19.
- **Extension upgrade mechanism.** A documented, forward-compatible example
  upgrade path (`pgokf--0.1.0--0.1.1.sql`) exercises
  `ALTER EXTENSION pgokf UPDATE` with a proven no-data-loss guarantee.

### Security

- Path traversal, symlink escape, NUL-byte, and relative-path inputs to
  `register_bundle` are rejected before any filesystem access.
- The `pgokf_private` schema and its `config` table are readable and writable
  only by the extension owner and `pgokf_admin`; readers cannot see policy.

[Unreleased]: https://github.com/LogicOcean/pgokf/compare/v0.1.7...HEAD
[0.1.7]: https://github.com/LogicOcean/pgokf/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/LogicOcean/pgokf/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/LogicOcean/pgokf/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/LogicOcean/pgokf/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/LogicOcean/pgokf/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/LogicOcean/pgokf/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/LogicOcean/pgokf/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/LogicOcean/pgokf/releases/tag/v0.1.0
