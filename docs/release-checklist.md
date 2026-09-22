# Release Checklist

The concrete, ordered steps to cut a `pgokf` release. Nothing here is
automatic: a release is a deliberate human decision. Work top to bottom; every
gate must pass before the next. The stability rules these gates enforce live in
[api-stability.md](api-stability.md), and every change must already be recorded
in [CHANGELOG.md](https://github.com/LogicOcean/pgokf/blob/main/CHANGELOG.md).

Throughout, `PGVER` is a PostgreSQL major (15–18) and `PG_CONFIG` is the path to
its `pg_config` (e.g. `/usr/lib/postgresql/18/bin/pg_config`).

## 1. Static quality gates

Run from the repository root. All must exit `0`.

```bash
cargo fmt --all -- --check
cargo clippy -p pgokf --no-default-features --features pg18 --all-targets -- -D warnings
cargo clippy --locked --workspace --exclude pgokf --all-targets -- -D warnings
scripts/test-workspace.sh
```

`scripts/test-workspace.sh` runs the complete workspace, including extension,
web and API tests. On native macOS it sets `LC_ALL=C` and appends
`-C link-arg=-Wl,-undefined,dynamic_lookup` to `RUSTFLAGS`: PostgreSQL resolves
extension symbols when loading the library. The equivalent direct invocation is:

```bash
LC_ALL=C RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C link-arg=-Wl,-undefined,dynamic_lookup" \
  cargo test --workspace --no-default-features --features pg18 --locked
```

Use the wrapper as the canonical local command on both Linux and macOS.
The in-database plain and provider-preloaded runs remain separate required gates.
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

All declared PG15–18 CI and packaging legs are required. Missing PGDG packages,
failed provisioning or absent base images fail the workflow; there is no
advisory success or skipped-coverage green. PG19 coverage is not established
until its pinned **19 Beta 3**, nonpublishing actual clippy and in-database test steps complete. A provisioning
failure blocks the support/release gate, even when other majors pass.
`python3 tests/test_ci_coverage.py` (requires PyYAML) executes the missing-package
failed-install and missing-image branches with local stubs and requires nonzero exits.

## 4. Per-major live smoke (repeat for PGVER = 15, 16, 17, 18)

Install into the target major, then create a scratch cluster whose socket path
stays short (the UNIX socket path limit is 107 bytes - keep it under a directory
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
WHERE n.nspname IN ('pgokf', 'pgokf_private', 'pgokf_web') AND c.relkind = 'r'
  AND obj_description(c.oid, 'pg_class') IS NULL;

-- All four API roles must be commented (expect four 't' rows):
SELECT r.rolname, shobj_description(r.oid, 'pg_authid') IS NOT NULL AS has_comment
FROM pg_roles r WHERE r.rolname IN
  ('pgokf_reader', 'pgokf_writer', 'pgokf_admin', 'pgokf_dispatcher') ORDER BY 1;
```

Each of the first three queries must return **no rows**. As a positive check,
this confirms full coverage (expect `70/70`, `24/24`, `29/29`: the 69 public
functions plus the internal `bm25_hits` helper, 24 composite types, and 29
catalog tables across `pgokf`, `pgokf_private`, and `pgokf_web`). The
in-database `every_catalog_object_carries_a_comment` test also checks
private helper functions.

```sql
SELECT 'functions',  count(*) FILTER (WHERE obj_description(p.oid,'pg_proc')  IS NOT NULL)||'/'||count(*)
FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='pgokf'
UNION ALL SELECT 'comp_types', count(*) FILTER (WHERE obj_description(t.oid,'pg_type') IS NOT NULL)||'/'||count(*)
FROM pg_type t JOIN pg_namespace n ON n.oid=t.typnamespace JOIN pg_class c ON c.oid=t.typrelid
WHERE n.nspname='pgokf' AND c.relkind='c'
UNION ALL SELECT 'tables', count(*) FILTER (WHERE obj_description(c.oid,'pg_class') IS NOT NULL)||'/'||count(*)
FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
WHERE n.nspname IN ('pgokf','pgokf_private','pgokf_web') AND c.relkind='r';
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
shipped chain runs one script per step from `0.1.0 → 0.1.1` through
`0.1.16 → 0.2.0` and on through the point-versioned development line
(`0.2.0 → 0.3.0-dev → 0.3.0-dev1 → … → 0.3.1`; see "Development point versions" in
[api-stability.md](api-stability.md)) to the current `default_version`.

> **0.1.3 was a breaking pre-release re-model.** The `pgokf.concept_provenance`
> shape changed to conform to OKF v0.2 (see [CHANGELOG.md](https://github.com/LogicOcean/pgokf/blob/main/CHANGELOG.md)).
> This remodel preceded the published 0.2.0 release. Its historical exception
> remains: `0.1.2 → 0.1.3` is **not** a no-data-loss in-place upgrade:
> re-`CREATE EXTENSION` and re-register bundles (the on-disk bundle is the
> source of truth, so the projection rebuilds fully from a sync). The
> no-data-loss upgrade guarantee below applies to every other link in the chain
> (the additive steps `0.1.0 → 0.1.1`, `0.1.1 → 0.1.2`, and `0.1.3` onward), and
> becomes a binding cross-version guarantee once `1.0.0` is cut.

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

### Repeatable upgrade-parity gate

For this PG18 gate, after installing the current checkout with
`cargo pgrx install`, place the
**authentic 0.2.0 install SQL** beside the current install SQL in the target
PostgreSQL's extension directory. For example, extract it from the published
0.2.0 PG18 image (do not rename the current install script to simulate an old
version):

```sh
docker run --rm --entrypoint cat ghcr.io/logicocean/pgokf:0.2.0-pg18 \
  /usr/share/postgresql/18/extension/pgokf--0.2.0.sql > /tmp/pgokf--0.2.0.sql
install -m 644 /tmp/pgokf--0.2.0.sql "$("$PG_CONFIG" --sharedir)/extension/pgokf--0.2.0.sql"
python3 scripts/upgrade-parity.py --pg-config "$PG_CONFIG"
```

The harness starts and removes its own local scratch cluster; it accepts no
production connection. It checks the installed upgrade scripts against this
checkout (a committed fresh install script must yield an identical catalog -
pgrx's entity ordering is unstable across invocations, so that comparison is
the resulting inventory, not bytes),
then compares fresh installation with normal, early-dev (missing
functions), hand-applied (non-member functions), compatible body drift in
members and nonmembers, and reordered ACL update routes from 0.2.0
through every development edge to `default_version`, plus one route per
deployed development point version (dev, dev1, dev2, dev3) reaching the
release through a bare `ALTER EXTENSION pgokf UPDATE`. Each route starts with
independent API roles, preserves all old columns of populated bundle, concept,
metadata and source rows, and verifies legacy bundles remain stale even after
a content resync. The [adoption policy](api-stability.md#development-function-adoption-security-policy)
is fail closed for both detached functions and members: all three signatures
must have canonical ownership and explicit grants. Security drift, incompatible
return/argument definitions and membership in another extension must abort without changing
ownership, ACLs, initial privileges, body, membership or extension version.

The catalog inventory compares function definitions and security settings,
comments, ownership, current and initial ACLs, relation/type/column definitions,
constraints (including domain constraints), enum labels in order, domain base
type/default/nullability/collation, indexes, views, rewrite rules, default ACLs,
policies, user triggers and internal FK-trigger state, sequences, roles,
extension membership and dump registration. Internal FK-trigger identities use
the parent constraint/table and trigger function, never generated OID-bearing
names. ACL entries are sorted before comparison; equivalent grant order is not
drift. Default ACLs include schema-scoped and global entries because both affect
future objects. Enum/domain definitions and domain constraints also cover
extension-owned types outside the three catalog schemas. Comments include constraints, policies, rules, triggers and the
extension itself.
Deliberate owner, comment, privilege, membership, column, security, policy, role and
dump-registration mutations must fail comparison, together with disabled FK
enforcement, rewrite rules, default ACLs, enum/domain changes and comment probes.
The API-stability suite
separately checks every historical edge from 0.1.0 and rejects missing targets,
append-without-bump and disconnected edges. Live data-preservation parity begins
at 0.2.0; it does not claim to execute the historical breaking 0.1.3 migration.

An upgrade script must never drop data-bearing objects, truncate, delete, or
rewrite existing catalog data (apart from the documented 0.1.3 remodel).
Replacing a function overload or widening a constraint in the same transaction
is allowed; neither carries catalog rows. `tests/api_stability.rs` enforces this on the shipped scripts.

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
- [ ] The Docker images build and pass their smoke tests locally
      (`packaging/docker/smoke-test.sh`, `smoke-test-companions.sh`); if
      `PG_TEXTSEARCH_VERSION` or `PG_SEARCH_VERSION` changed, the matching
      `packaging/docker/pg_textsearch.sha256` / `pg_search.sha256` table was
      regenerated.

## 7. Version bump, changelog, tag

- [ ] Bump `version` in the workspace `Cargo.toml` (and `default_version` in
      `pgokf.control`) as a **single, deliberate commit**, together with every
      other pin of the version: `META.json`, `packaging/rpm/pgokf.spec`,
      the `=<version>` path-dependency pins in
      the companion crates, the image tags in `deploy/compose/.env.example`
      and the `packaging/docker/*` / `docs/` examples, and `Cargo.lock`
      (`cargo update --workspace`).
- [ ] **Collapse a point-versioned dev cycle.** Releasing out of a
      `X.Y.0-devN` line means: the bump above lands the clean `X.Y.0`, and one
      terminal `sql/pgokf--X.Y.0-devN--X.Y.0.sql` script (a no-op beyond the
      standing `register_dump_relations()` call) ships so point-versioned
      deployments reach the release through `ALTER EXTENSION pgokf UPDATE`
      like any other step. Then prove the released chain converges with a
      fresh install: upgrade a scratch cluster from the previous tag through
      every dev step to `X.Y.0` and diff the object catalog against a fresh
      `CREATE EXTENSION` at `X.Y.0`.
- [ ] Move the `Unreleased` changelog section under the new version with the
      release date; add fresh compare/tag links.
- [ ] Re-run gates 1–5 against the bumped version.
- [ ] Tag `vX.Y.Z` and push the tag.
- [ ] Keep the runnable Homebrew formula on the last published release through
      this tag. Follow the separate post-tag main/tap sequence below.

## 8. Publish

- [ ] Attach the per-major `cargo pgrx package` artifacts to the release.
- [ ] Confirm the `packages` workflow run for the tag published the
      multi-architecture images (`ghcr.io/logicocean/pgokf:<version>-pg<major>`
      and `ghcr.io/logicocean/pgokf-companions:<version>`, each listing
      `linux/amd64` and `linux/arm64` in `docker buildx imagetools inspect`).
- [ ] Publish to [PGXN](https://pgxn.org): update `META.json` (name, version,
      abstract, `provides`, license, resources), build the release zip, and
      upload. Confirm the version and extension name match the tag.
- [ ] Complete the post-tag main and external Homebrew tap sequence below.
- [ ] Announce in the changelog links and the repository release notes.

## Homebrew formula and tap

The source release and its consuming formula have separate lifecycles. The
pre-tag 0.3.1 tree and immutable `v0.3.1` tag keep the runnable formula pinned
to **v0.2.0**, its authenticated archive digest, and matching 0.2.0 test
assertions. This is intentional: a formula cannot contain the digest of the
archive containing itself. The external distribution is
[`LogicOcean/homebrew-pgokf`](https://github.com/LogicOcean/homebrew-pgokf).

After all release gates and independent review, the release operator performs
these steps in order (these are publication instructions, not local gates):

1. Tag the reviewed source commit as `v0.3.1` and push that immutable tag.
   Record `git rev-parse 'v0.3.1^{commit}'`. Never move this tag.
2. Download and inspect its archive, failing on HTTP errors:

   ```bash
   curl --fail --location --retry 3 \
     https://github.com/LogicOcean/pgokf/archive/refs/tags/v0.3.1.tar.gz \
     --output /tmp/pgokf-v0.3.1.tar.gz
   shasum -a 256 /tmp/pgokf-v0.3.1.tar.gz
   tar -tzf /tmp/pgokf-v0.3.1.tar.gz
   tar -xOf /tmp/pgokf-v0.3.1.tar.gz pgokf-0.3.1/crates/extension/pgokf.control
   ```

   Verify archive root `pgokf-0.3.1`, control/workspace/PGXN version 0.3.1,
   and unpacked tracked contents against the tagged commit. Record the exact
   archive bytes and SHA256 in release evidence; do not hash a local repack.
3. On **main after the tag**, update `packaging/homebrew/pgokf.rb` URL,
   SHA256, both `test do` version assertions, and its release-state comment
   together. Add the authenticated `0.3.1` digest to `KNOWN_RELEASE_DIGESTS`
   in `tests/test_release_integrity.py` in the same commit. No placeholder is
   allowed. Run `python3 tests/test_release_integrity.py` and
   `python3 tests/test_release_tools.py`. This commit consumes the earlier
   immutable tag; do not retag it.
4. Copy that exact formula to `Formula/pgokf.rb` in a local checkout of
   `LogicOcean/homebrew-pgokf`. Verify `cmp` against the main formula, run
   `brew audit --strict Formula/pgokf.rb`, then in the local tap run
   `brew install --build-from-source logicocean/pgokf/pgokf` and
   `brew test logicocean/pgokf/pgokf`. The formula test creates an isolated
   cluster and checks the installed control and extension version. Publish
   the main commit and then the tested external tap commit only after these
   checks pass. The formula inside the source tag remains the valid 0.2.0 pin.
5. Verify the external tap fetches the recorded archive digest and the
   installed extension reports 0.3.1. Record both main and tap commit SHAs.

Rollback: before a valid archive exists, leave the old formula unchanged.
If validation fails before pushing, correct the local post-tag changes or
restore the entire old URL/digest/assertion tuple. If a bad post-tag formula
has already been published, revert the complete main and tap formula commits
together to the last verified tuple; never publish a mixed tuple, delete or
move the source tag, or substitute bytes under it. A source defect requires
a new release version. A checksum discrepancy requires investigation against
the recorded archive and tag contents before any formula update.

An authorized current-release retry may name only the exact current tag:
`gh workflow run packages.yml --ref main -f release_tag=v0.3.1`.
Prep retains current resolver tooling separately, checks out the tag source,
proves HEAD/tag/control/workspace/lock/PGXN identity, and passes the resolved
commit SHA to all builds. Empty dispatch input is a non-publishing smoke run.
A branch source cannot publish release image names. Historical catch-up is unsupported:
0.2.0 remains the published predecessor, 0.3.0 remains rejected, and 0.3.1 is
the only next eligible release. Current machinery refuses historical source
before any package upload, image push, or manifest job can run.

Extract the exact GitHub release body, review it, and then create the release
only after the publication gates:

```bash
awk '/^## \[0\.3\.1\]/{emit=1; next} emit && /^## \[/{exit} emit {print}' \
  CHANGELOG.md > /tmp/pgokf-0.3.1-release.md
# Inspect the body and attach the verified per-major assets as appropriate.
gh release create v0.3.1 --verify-tag --title 'pgokf 0.3.1' \
  --notes-file /tmp/pgokf-0.3.1-release.md
```

## Quick gate summary

| Gate | Command / check | Pass condition |
| ---- | --------------- | -------------- |
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy … -D warnings` | exit 0 |
| Tests | `scripts/test-workspace.sh` | all pass, incl. `api_stability` |
| In-database | `RUST_TEST_THREADS=1 cargo pgrx test pg18 …`, once plain and once with `PGOKF_TEST_PRELOAD=pg_textsearch,pg_search` | all pass; the provider tests run (not skip) in the preloaded run |
| Supply chain | `cargo deny check`, `cargo audit --deny warnings` | no denials/advisories |
| Schema | `cargo pgrx schema pg18` | builds; comments present |
| Live smoke | `CREATE EXTENSION` on each major 15–18 | functions work |
| COMMENT coverage | `obj_description` queries (§4a) | zero uncommented objects |
| Upgrade | `ALTER EXTENSION … UPDATE` (§5) | version advances, no data loss |
| Packaging | `cargo pgrx package` | tree per major |
| Release | version bump, tag, PGXN | tag == control == crate version |

Before tagging, run the clean-commit arm64 macOS source-mechanics gate
`scripts/test-homebrew-packaging.py NEW_EVIDENCE_DIRECTORY`, plus the portable
`tests/test_homebrew_packaging.py` and `tests/test_cleanup_ownership.py` guards.
Retain the permanent cleanup evidence limitation in
[the recovery record](releases/0.3.1-recovery.md#permanent-historical-cleanup-evidence-limitation).
