# Multi-tenancy

`pgokf` supports **opt-in, row-level multi-tenant isolation**: one catalog can
hold many tenants' bundles side by side, and a session sees only its own
tenant's data — or, when it declares no tenant, *all* of it. The feature is
strictly backward compatible: an existing install, and any session that never
sets a tenant, behaves exactly as it did before multi-tenancy was added.

It is built from two ordinary PostgreSQL primitives — a per-session GUC and
row-level security (RLS) — so there is no new API surface to learn and nothing
to turn on at install time.

## The model in one paragraph

Every projection table carries a denormalized `tenant_id text NOT NULL DEFAULT
'default'`. A session selects its tenant with the `pgokf.tenant` GUC. Each table
has an RLS policy whose predicate is:

```sql
current_setting('pgokf.tenant', true) IS NULL
   OR current_setting('pgokf.tenant', true) = ''
   OR tenant_id = current_setting('pgokf.tenant', true)
```

So a session that has **not** set `pgokf.tenant` (the value is unset or empty)
matches every row — the pre-multi-tenancy behavior — while a session that **has**
set it matches only that tenant's rows. Writes stamp the row's `tenant_id` from
the same GUC, and a bundle is single-tenant: all of its concepts, links,
provenance, embeddings, and stored source inherit the bundle's tenant.

## Selecting a tenant

`pgokf.tenant` is a normal `USERSET` GUC, so any of the standard mechanisms work:

```sql
-- Per session (a connection pool would issue this on checkout):
SET pgokf.tenant = 'acme';

-- Pinned to a login role, so every connection as that role is scoped:
ALTER ROLE acme_app SET pgokf.tenant = 'acme';

-- As a connection option (libpq), never touching SQL:
--   options='-c pgokf.tenant=acme'
```

Unset it — returning the session to the see-all default — with `SET pgokf.tenant
= ''` or `RESET pgokf.tenant`.

There is no separate "create tenant" step: a tenant exists exactly as long as it
owns at least one bundle. Registering a bundle under `pgokf.tenant = 'acme'`
creates the `acme` tenant implicitly.

## Reads: automatic and transparent

The reader functions — `concept_search`, `concept_search_semantic` /
`concept_search_hybrid`, `find_similar`, `concept_neighbors`, `list_bundles`,
`bundle_info`, `catalog_stats`, `stale_concepts`, `get_concept_source` — run with
**invoker rights** over the base tables, so RLS filters them automatically. No
argument changes; a scoped session simply sees a smaller catalog.

Two readers are `SECURITY DEFINER` and therefore bypass RLS, so they apply the
identical tenant predicate **explicitly** instead: `list_sync_log` filters its
rows, and `health`'s `bundle_count` / `concept_count` are tenant-scoped. Both
still see everything for an unset session.

## Writes: stamped from the session

Every write goes through a `SECURITY DEFINER` sync/admin function that runs as
the extension owner and so bypasses RLS — correct, because each operates strictly
within one single-tenant bundle. The bundle row is stamped with
`effective_tenant()` (the GUC, or `'default'` when unset), and every child row
inherits the bundle's `tenant_id`. `refresh_bundle`, `unregister_bundle`, and
`set_bundle_enabled` operate on an existing bundle by its surrogate id and never
change its tenant.

## Write confinement

Setting `pgokf.tenant` confines writes as tightly as it confines reads. The
`SECURITY DEFINER` functions that take an explicit `bundle_id` — `refresh_bundle`,
`unregister_bundle`, `set_bundle_enabled`, `set_concept_embedding`, and the admin
exports `export_parquet` / `export_sources` — run as the table owner and so bypass
RLS. On its own that would let a `pgokf_writer` / `pgokf_admin` session which has
`SET pgokf.tenant = 'acme'` reach another tenant's bundle just by passing its id.
Each of those functions therefore applies an explicit guard the moment the
`bundle_id` is known, before any lock, file, or catalog side effect:

- when `pgokf.tenant` is **set**, the target bundle must belong to that tenant. A
  bundle owned by any other tenant is rejected with the *same* SQLSTATE `22023`
  "bundle … is not registered" error a genuinely unknown id raises — so a
  cross-tenant id is **indistinguishable from a nonexistent one** and cannot be
  used to probe whether another tenant holds a bundle;
- when `pgokf.tenant` is **unset or empty**, nothing is restricted: the session is
  cross-tenant by design, exactly as the read policy's "unset = see all". This is
  the trusted operator/superuser path, and it preserves the pre-multi-tenancy
  behavior.

The guard is ordinary code, not RLS, so it holds even for a superuser or the
extension owner — precisely the callers RLS lets through. Write confinement thus
equals read confinement: with a tenant set, a session can only mutate or export
the bundles it can see; with no tenant set, it can operate on all of them.
`register_bundle` / `register_bundle_content` are deliberately *not* guarded this
way — they **create** a bundle stamped with the session's tenant, so registering
the same path under a different tenant is the intended per-tenant behavior (see
below), not a cross-tenant write.

## Per-tenant bundle keys

The bundle registration key is `UNIQUE (tenant_id, path)`, not `UNIQUE (path)`.
Two tenants can therefore register the **same** path — a filesystem root or a
`content:<name>` key — as independent bundles:

```sql
SET pgokf.tenant = 'acme';
SELECT pgokf.register_bundle('/srv/bundles/handbook');   -- acme's bundle

SET pgokf.tenant = 'globex';
SELECT pgokf.register_bundle('/srv/bundles/handbook');   -- globex's own bundle
```

The duplicate-registration check (`23505`) is scoped to the current tenant, so
re-registering *your own* tenant's path is still rejected with the usual
"use `refresh_bundle`" guidance.

## Why the SECURITY DEFINER write functions may bypass RLS

RLS is **enabled but not forced** on the projection tables, so the table owner
(and thus every `SECURITY DEFINER` function) bypasses it. This is deliberate and
safe: those functions never mix tenants in one statement — they read and write
strictly within a single bundle, which is single-tenant — and they are the only
paths that write. Forcing RLS on the owner would break the write path (which must
stamp `tenant_id` and read across the bundle it owns) without adding isolation
that the single-bundle scoping does not already guarantee.

## The trust model: what the `pgokf.tenant` GUC does and does not contain

State this plainly, because it governs how the feature may safely be deployed:

> **`pgokf.tenant` is a scoping selector, not a hard security boundary against a
> tenant who can execute arbitrary SQL.**

`pgokf.tenant` is a `USERSET` GUC, so **any session that can run SQL can change
its own value** — with `SET pgokf.tenant = 'other'`, `RESET pgokf.tenant`, or
`SELECT set_config('pgokf.tenant', '', false)` — to another tenant's value or to
empty, which the policy treats as *see-all*. Pinning it with `ALTER ROLE acme_app
SET pgokf.tenant = 'acme'` sets only a **session default**: a subsequent plain
`SET pgokf.tenant` in that same session overrides it, and RLS then filters by the
new value. So the GUC contains an **honest, cooperating** client — one that
issues no `SET` of its own and simply inherits the scope it is given — but it does
**not** contain a **hostile** tenant who can submit raw SQL. This is inherent to
any GUC-based tenancy, not a defect in the RLS policies (which are correct); the
GUC is simply the wrong anchor for a boundary the adversary can move.

Also note the **fail-open default**: unset (or empty) `pgokf.tenant` means *see
every row*. A tenant-facing connection that is not forced to carry a tenant, and
forgets to set one, sees the whole catalog.

### Getting a *hard* boundary

To make tenant isolation a real security boundary against an untrusted tenant,
the tenant must not be able to run arbitrary `SET` / SQL against the database.
Use one of:

- **A constrained access layer.** Let the tenant reach PostgreSQL only through a
  trusted connection pooler or a restricted API that pins `pgokf.tenant` on every
  checkout and refuses to pass through raw `SET` or ad-hoc SQL. The GUC is then
  set by infrastructure the tenant cannot influence.
- **A per-tenant database role.** Give each tenant its own login role and let
  ordinary PostgreSQL privileges — not a session GUC — enforce isolation
  (optionally combined with `FORCE ROW LEVEL SECURITY` and per-tenant grants).
  This holds even against a tenant issuing arbitrary SQL, because the role, not
  the GUC, is the boundary.

Without one of these, treat `pgokf.tenant` as convenience scoping among *trusted*
callers, not as isolation against a hostile one.

## Operational hardening (for the cooperating-client model)

Even within the cooperating-client model above, these reduce accidental
cross-tenant exposure:

- **Pin the tenant to the role or connection**, not to ad-hoc `SET` statements:
  `ALTER ROLE acme_app SET pgokf.tenant = 'acme'`, or a connection-string option.
  This stops an honest client from *accidentally* running unscoped; it does not
  stop one that deliberately issues its own `SET` (see the trust model above).
- **Never leave `pgokf.tenant` unset for a tenant-facing connection.** Reserve the
  unset (see-all) session for a trusted operator/admin.
- **Reads run as a non-superuser.** RLS is bypassed by superusers and the table
  owner. A tenant application must connect as an ordinary login role that is a
  member of `pgokf_reader` (or `pgokf_writer`), not as a superuser and not as the
  extension owner.
- The by-id mutators and exports are **tenant-confined** for a scoped session
  (see [Write confinement](#write-confinement)): a cross-tenant id is rejected as
  an unknown bundle, so it is not a cross-tenant write vector even though it
  bypasses RLS. A tenant also only sees the bundle ids it is allowed to see
  (`list_bundles` is RLS-filtered). Reserve the unset (see-all) session — which is
  cross-tenant for *both* reads and writes — for a trusted operator, and continue
  to treat `pgokf_writer` / `pgokf_admin` as trusted tiers.

## Upgrading an existing catalog

`ALTER EXTENSION pgokf UPDATE TO '0.1.7'` adds the `tenant_id` column to every
projection table (backfilling all existing rows to `'default'`), swaps the
bundles key to `UNIQUE (tenant_id, path)`, and enables the RLS policies. Because
the policy is a no-op for a session that sets no tenant, **the upgraded catalog
behaves identically to before** until a session opts in by setting
`pgokf.tenant`. No data is moved or lost.

## Limits and non-goals

- Isolation is per **row**, enforced by RLS — it is not physical separation.
  Tenants share tables, indexes, and the buffer cache. For hard physical
  separation, use separate databases or clusters.
- The `pgokf_private.config` policy row and the resource-ceiling GUCs are
  **cluster-global**, not per tenant.
- A bundle belongs to exactly one tenant for its whole life; there is no
  "move a bundle to another tenant" operation (unregister and re-register under
  the new tenant instead).
