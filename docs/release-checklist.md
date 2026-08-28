# Release Checklist

The concrete, ordered steps to cut a `pgokf` release. Nothing here is
automatic: a release is a deliberate human decision. Work top to bottom; every
gate must pass before the next. The stability rules these gates enforce live in
[api-stability.md](api-stability.md), and every change must already be recorded
in [CHANGELOG.md](https://github.com/LogicOcean/okf-pg-catalog/blob/main/CHANGELOG.md).

Throughout, `PGVER` is a PostgreSQL major (15–19) and `PG_CONFIG` is the path to
its `pg_config` (e.g. `/usr/lib/postgresql/18/bin/pg_config`).

## 1. Static quality gates

Run from the repository root. All must exit `0`.

```bash
cargo fmt --all -- --check
cargo clippy -p pgokf --no-default-features --features pg18 --all-targets -- -D warnings
cargo test  -p pgokf --no-default-features --features pg18
```

`cargo test` includes `tests/api_stability.rs`, which fails if any public
object lacks a `COMMENT ON` or if the locked public-function count drifts.

## 2. Supply-chain gates

```bash
cargo deny check          # licenses, bans, advisories, sources (deny.toml)
cargo audit               # RUSTSEC advisories against Cargo.lock
```

## 3. Schema generation

Confirm the SQL entity graph builds and inspect the diff for unintended
surface changes:

```bash
cd crates/extension && cargo pgrx schema pg18
```

The output must contain a `COMMENT ON` for every public function, type, and
table (the `version_comment` finalize block is the last entity emitted).

## 4. Per-major live smoke (repeat for PGVER = 15, 16, 17, 18, 19)

Install into the target major, then create a scratch cluster whose socket path
stays short (the UNIX socket path limit is 107 bytes — keep it under a directory
like `/tmp/…`, never a deep project path):

```bash
cd crates/extension
cargo pgrx install --no-default-features --features pg${PGVER} \
    --pg-config ${PG_CONFIG} --sudo

DATA=/tmp/pgokf-rel/data; SOCK=/tmp/pgokf-rel/s
rm -rf /tmp/pgokf-rel && mkdir -p "$DATA" "$SOCK"
${PG_BIN}/initdb -D "$DATA" -U postgres --auth=trust
${PG_BIN}/pg_ctl -D "$DATA" -o "-c listen_addresses='' -k $SOCK" -w start
PSQL="${PG_BIN}/psql -h $SOCK -U postgres -d postgres -v ON_ERROR_STOP=1"
```

### 4a. Install and the COMMENT-coverage gate (must return zero rows)

```sql
CREATE EXTENSION pgokf;

-- Uncommented public functions:
SELECT n.nspname||'.'||p.proname||'('||pg_get_function_identity_arguments(p.oid)||')'
FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
WHERE n.nspname = 'pgokf' AND obj_description(p.oid, 'pg_proc') IS NULL;

-- Uncommented public composite types:
SELECT n.nspname||'.'||t.typname
FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
JOIN pg_class c ON c.oid = t.typrelid
WHERE n.nspname = 'pgokf' AND c.relkind = 'c'
  AND obj_description(t.oid, 'pg_type') IS NULL;

-- Uncommented catalog tables (public + private):
SELECT n.nspname||'.'||c.relname
FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname IN ('pgokf', 'pgokf_private') AND c.relkind = 'r'
  AND obj_description(c.oid, 'pg_class') IS NULL;

-- Both API roles must be commented (expect two 't' rows):
SELECT r.rolname, shobj_description(r.oid, 'pg_authid') IS NOT NULL AS has_comment
FROM pg_roles r WHERE r.rolname LIKE 'pgokf_%' ORDER BY 1;
```

Each of the first three queries must return **no rows**. As a positive check,
this confirms full coverage (expect `14/14`, `5/5`, `9/9` — 14 functions, 5
composite types, and 9 catalog tables = the 8 public `pgokf` tables plus
`pgokf_private.config`):

```sql
SELECT 'functions',  count(*) FILTER (WHERE obj_description(p.oid,'pg_proc')  IS NOT NULL)||'/'||count(*)
FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='pgokf'
UNION ALL SELECT 'comp_types', count(*) FILTER (WHERE obj_description(t.oid,'pg_type') IS NOT NULL)||'/'||count(*)
FROM pg_type t JOIN pg_namespace n ON n.oid=t.typnamespace JOIN pg_class c ON c.oid=t.typrelid
WHERE n.nspname='pgokf' AND c.relkind='c'
UNION ALL SELECT 'tables', count(*) FILTER (WHERE obj_description(c.oid,'pg_class') IS NOT NULL)||'/'||count(*)
FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
WHERE n.nspname IN ('pgokf','pgokf_private') AND c.relkind='r';
```

### 4b. Functional smoke

```sql
SELECT * FROM pgokf.register_bundle('/abs/path/to/examples/sample-bundle'); -- added=4
SELECT concept_id, title FROM pgokf.concept_search('postgres failover');    -- ranked hits
SELECT * FROM pgokf.concept_neighbors('runbooks/database-failover', 2);      -- graph walk
SELECT pgokf.version();                                                      -- library version
```

## 5. Upgrade path

The extension ships forward-compatible upgrade scripts named
`sql/pgokf--<from>--<to>.sql`. `cargo pgrx install` writes the full install
script as `pgokf--<crate-version>.sql` and copies every upgrade script
alongside it, so the update path is available without any manual step. The
shipped chain is `0.1.0 → 0.1.1 → 0.1.2 → 0.1.3`.

> **0.1.3 is a breaking pre-release re-model.** The `pgokf.concept_provenance`
> shape changed to conform to OKF v0.2 (see [CHANGELOG.md](https://github.com/LogicOcean/okf-pg-catalog/blob/main/CHANGELOG.md)).
> Because the extension is still pre-release with no tagged release and no
> external installs, `0.1.2 → 0.1.3` is **not** a no-data-loss in-place upgrade:
> re-`CREATE EXTENSION` and re-register bundles (the on-disk bundle is the
> source of truth, so the projection rebuilds fully from a sync). The
> no-data-loss upgrade guarantee below applies to the additive `0.1.0 → 0.1.1`
> and `0.1.1 → 0.1.2` links, and becomes a binding cross-version guarantee once
> `1.0.0` is cut.

Verify an upgrade preserves a populated catalog byte-for-byte:

```sql
-- Populate at the current version, capture fingerprints:
CREATE EXTENSION pgokf VERSION '0.1.0';
GRANT pgokf_admin TO postgres; SET ROLE pgokf_admin;
SELECT * FROM pgokf.register_bundle('/abs/path/to/examples/sample-bundle');
SELECT count(*) FROM pgokf.concepts;                       -- e.g. 4
SELECT md5(string_agg(id||':'||file_hash,',' ORDER BY bundle_id,id)) FROM pgokf.concepts;

-- Upgrade and re-check: extversion advances, every count and fingerprint holds:
ALTER EXTENSION pgokf UPDATE TO '0.1.1';
SELECT extversion FROM pg_extension WHERE extname = 'pgokf';   -- 0.1.1
SELECT count(*) FROM pgokf.concepts;                           -- unchanged
SELECT md5(string_agg(id||':'||file_hash,',' ORDER BY bundle_id,id)) FROM pgokf.concepts; -- unchanged
```

> Note: bare `ALTER EXTENSION pgokf UPDATE` (no `TO`) targets the control file's
> `default_version`. When a real release advances the version, bump
> `default_version` in `pgokf.control` together with the crate version and ship a
> matching full install script; until then, target the version explicitly with
> `UPDATE TO`.

An upgrade script must never `DROP`, `TRUNCATE`, `DELETE`, or rewrite existing
catalog data. `tests/api_stability.rs` enforces this on the shipped scripts.

Tear down each scratch cluster when done:

```bash
${PG_BIN}/pg_ctl -D "$DATA" -w stop -m fast && rm -rf /tmp/pgokf-rel
```

## 6. Packaging

- [ ] `pgokf.control`: `default_version` matches the release, `comment`,
      `superuser`, `relocatable`, and `trusted` are correct.
- [ ] Crate/workspace `version` matches the release; `pgokf.version()` returns
      it (it reads `CARGO_PKG_VERSION`).
- [ ] `sql/pgokf--<from>--<to>.sql` upgrade script from the previous release
      exists and passed the upgrade gate above.
- [ ] `cargo pgrx package --pg-config ${PG_CONFIG}` produces the install tree
      for each supported major.

## 7. Version bump, changelog, tag

- [ ] Bump `version` in the workspace `Cargo.toml` (and `default_version` in
      `pgokf.control`) as a **single, deliberate commit**.
- [ ] Move the `Unreleased` changelog section under the new version with the
      release date; add fresh compare/tag links.
- [ ] Re-run gates 1–5 against the bumped version.
- [ ] Tag `vX.Y.Z` and push the tag.

## 8. Publish

- [ ] Attach the per-major `cargo pgrx package` artifacts to the release.
- [ ] Publish to [PGXN](https://pgxn.org): update `META.json` (name, version,
      abstract, `provides`, license, resources), build the release zip, and
      upload. Confirm the version and extension name match the tag.
- [ ] Announce in the changelog links and the repository release notes.

## Quick gate summary

| Gate | Command / check | Pass condition |
| ---- | --------------- | -------------- |
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy … -D warnings` | exit 0 |
| Tests | `cargo test -p pgokf …` | all pass, incl. `api_stability` |
| Supply chain | `cargo deny check`, `cargo audit` | no denials/advisories |
| Schema | `cargo pgrx schema pg18` | builds; comments present |
| Live smoke | `CREATE EXTENSION` on each major 15–19 | functions work |
| COMMENT coverage | `obj_description` queries (§4a) | zero uncommented objects |
| Upgrade | `ALTER EXTENSION … UPDATE` (§5) | version advances, no data loss |
| Packaging | `cargo pgrx package` | tree per major |
| Release | version bump, tag, PGXN | tag == control == crate version |
