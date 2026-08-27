# pgokf security model

`pgokf` treats a filesystem path as **privileged server-side input**: the
PostgreSQL backend reads arbitrary files from the host filesystem on behalf of a
SQL caller (bundle registration and refresh), and in exactly one function —
`export_parquet` — **writes** files back to it. The security model exists to make
both safe: to bound which files can be read or written, who can trigger the
access, and what a bundle's contents can and cannot do inside the database. This
document explains each mechanism and the reasoning behind it.

Every role, grant, and behavior described here is enforced in the extension
source (`crates/extension/src/security.rs`, `sql/bootstrap.sql`, and the
per-feature `catalog/*.rs` hardening blocks) and was exercised against a live
cluster.

## Threat model

The database never executes bundle content. Markdown text, YAML scalar values,
link destinations, and referenced resources are **data only** — parsed, stored,
and indexed, never interpreted as commands. The mechanisms below defend against:

- reading files outside an intended directory (path traversal, symlink escape);
- **writing** an export outside an intended directory, or writing at all as a
  non-admin (`export_parquet` is the sole file-writing surface — see
  [Server-side file writes](#server-side-file-writes-export_parquet));
- an unprivileged user registering, refreshing, reconfiguring, or exporting the
  catalog;
- SQL injection through concept content or configuration values;
- privilege escalation through the `SECURITY DEFINER` functions;
- resource exhaustion from oversized or oversized-count bundles (see
  [configuration.md](configuration.md) for the GUC ceilings).

## Roles and least privilege

Two cluster-wide `NOLOGIN` roles are created idempotently at extension install
(`sql/bootstrap.sql`). You grant them to real login users; nobody logs in *as*
them.

| Role | Capabilities |
| ---- | ------------ |
| `pgokf_reader` | `USAGE` on schema `pgokf`; `SELECT` on the projection tables; `EXECUTE` on read paths: `concept_search`, `concept_neighbors`, `list_bundles`, `bundle_info`, `get_config`. |
| `pgokf_admin` | Inherits `pgokf_reader` (it is `GRANT`ed the reader role), plus `USAGE` on `pgokf_private` and `EXECUTE` on the mutators: `register_bundle`, `refresh_bundle`, `unregister_bundle`, `set_config`, `reset_config`, and `export_parquet` (the file-writing export). |

`PUBLIC` is stripped: `REVOKE ALL ON SCHEMA pgokf FROM PUBLIC` and
`REVOKE ALL ON SCHEMA pgokf_private FROM PUBLIC` run first, and every function
does `REVOKE ALL … FROM PUBLIC` before granting `EXECUTE` to exactly the role
that needs it. The one deliberate exception is `pgokf.version()`, which exposes
only the crate version string and is left executable by everyone.

Because the roles are separable, an analytics user can be granted read-only
search access without ever being able to register a bundle, mutate the catalog,
or read the private configuration schema.

## Defense in depth: grants plus an in-function check

Authorization is enforced at **two independent layers**:

1. **SQL `EXECUTE` grants** — a caller without the grant is rejected by
   PostgreSQL before the function body runs (`42501 permission denied for
   function …`).
2. **An in-function role check** — every entry point calls
   `security::authorize_current_user(Operation::{Register|Refresh|Search}, …)`,
   which evaluates `pg_has_role` and raises `42501` when membership is missing.

The two layers are intentionally redundant: a future grant mistake, a `SECURITY
DEFINER` boundary, or a superuser path cannot silently bypass the policy, because
the role check runs regardless of how execution was reached.

Observed on a live cluster:

```text
-- reader (or an unprivileged user) invoking an admin mutator:
ERROR:  42501: permission denied for function register_bundle
-- an unprivileged user invoking search:
ERROR:  42501: permission denied for function concept_search
```

### `session_user`, not `current_user`

The in-function check evaluates membership for **`session_user`**, not
`current_user`. Inside a `SECURITY DEFINER` function `current_user` is the
function owner (the extension owner), which would make the check meaningless.
`session_user` still identifies the invoking session, and a session can only
`SET ROLE` to a role it already belongs to, so keying on it never widens access.
Superusers pass `pg_has_role` for every role, exactly as everywhere else in
PostgreSQL.

The membership lookup runs through **read-only** SPI (`SpiClient::select`) so it
is safe to call from `STABLE, PARALLEL SAFE` functions such as `concept_search`
and `concept_neighbors`; a writable SPI call would try to assign a transaction
ID, which is an error inside a parallel worker.

## `SECURITY DEFINER` and the pinned `search_path`

The five mutators (`register_bundle`, `refresh_bundle`, `unregister_bundle`,
`set_config`, `reset_config`), `get_config`, and the `export_parquet` snapshot
export are `SECURITY DEFINER`. They must be: write access to the base tables and
the private config schema stays with the extension owner, and callers never
receive direct DML — they mutate only through these audited entry points.
`export_parquet` reads the full catalog projection and so runs as the owner for
the same reason, while its file-write side is bounded separately (below).

Every `SECURITY DEFINER` function is created with a **pinned search path**:

```sql
ALTER FUNCTION pgokf.register_bundle(text, text, jsonb)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
```

Pinning `search_path = pg_catalog, pg_temp` closes the classic definer-function
attack where a caller creates a same-named function or operator in a schema
earlier on the search path and hijacks an unqualified reference inside the
definer body. With the path pinned to `pg_catalog` (and `pg_temp` last), name
resolution cannot be redirected by the caller. Consistent with this, the
extension's own SQL references built-ins fully qualified (`pg_catalog.now()`,
`pg_catalog.to_tsvector`, `pg_catalog.pg_advisory_xact_lock`, …).

The read-only search and graph functions run with **invoker rights** on purpose:
they read only tables that `pgokf_reader` already holds `SELECT` on, so escalating
to the owner would grant nothing and would only widen the attack surface. Row
access there obeys ordinary PostgreSQL permissions, with the in-function role
check as an added guard.

## Path validation and symlink-escape containment

Before any filesystem access, a caller-supplied bundle path passes
`security::validate_path_syntax`, which rejects:

- **relative paths** — the path must be absolute (`22023: path must be
  absolute`);
- **`..` traversal** — any parent-directory component is refused *before*
  canonicalization (`22023: path traversal is not allowed`);
- **NUL bytes** — refused outright.

The path is then canonicalized (`std::fs::canonicalize`) and confirmed to be a
directory; the resolved canonical path is what gets stored and used for
advisory-lock keying. During discovery, the sync engine (`okf_sync::discover`)
rejects any symlink whose target escapes the canonical bundle root — a symlink
inside the bundle pointing outside it cannot be used to read arbitrary files.

### `allowed_roots` containment

When one or more `allowed_roots` are configured (see
[configuration.md](configuration.md)), registration additionally requires the
resolved path to fall **inside** one of them, via
`security::canonicalize_contained_path`. That function canonicalizes **both**
the candidate path and each allowed root before comparing, so containment cannot
be escaped by a symlink on either side. A path that resolves outside every root
is rejected:

```text
ERROR:  22023: resolved path /tmp/.../outside-bundle is outside allowed_roots
```

When no roots are configured, the interim policy applies: any absolute,
canonical, traversal-free directory is accepted — and registration is still
restricted to `pgokf_admin`. Configuring `allowed_roots` is the recommended
hardening step for any multi-tenant or shared cluster.

`allowed_roots` entries are themselves validated as absolute, traversal-free
paths when set, so a malformed root cannot be stored.

## Server-side file writes (`export_parquet`)

Every function described so far only *reads* the filesystem. `export_parquet`
(`crates/extension/src/catalog/export.rs`) is the **single exception**: it writes
one Apache Parquet file per catalog table for a bundle into a server-side
directory. Because a file *write* from inside the backend is strictly more
dangerous than a read, it is guarded at least as tightly as registration:

- **Admin-only.** It is `SECURITY DEFINER`, `GRANT`ed `EXECUTE` to `pgokf_admin`
  alone, and calls `authorize_current_user(Operation::Register, …)` in its body,
  so a reader can neither be granted it accidentally nor reach it through the
  definer boundary. No reader-executable path writes files.
- **Destination validated like a bundle root.** `dest_dir` passes the same
  `validate_path_syntax` (absolute, NUL-free, traversal-free) and is
  canonicalized so a symlink cannot redirect the write. When `allowed_roots` is
  configured, the canonical directory must be contained within a configured root
  via `canonicalize_contained_path`, which resolves symlinks on **both** sides.
- **No directory creation, no writes outside the target.** The directory must
  already exist and be writable; the function never creates a directory and never
  writes anywhere but the four fixed file names inside the validated directory. A
  directory the server process cannot write fails with `42501`; a bad, missing,
  or non-contained directory fails with `22023`.
- **Bounded and bundle-scoped.** Each table is streamed in bounded keyset
  batches (peak memory independent of catalog size), and every query is scoped to
  the requested `bundle_id`, so an export cannot leak another bundle's rows.

**Residual risk:** when no `allowed_roots` are configured, the interim policy
accepts any absolute, canonical, traversal-free, writable directory on the server
— which is precisely why the function is gated to `pgokf_admin`. Operators who
want a hard filesystem boundary for exports (as for reads) should configure
`allowed_roots`.

## Injection safety: parameterized SPI only

Every value that originates from bundle content or caller input reaches SQL
**exclusively as a bound parameter** (`Spi::run_with_args`,
`SpiClient::select`/`update` with an argument list) — never through string
interpolation. Concept titles, bodies, tags, link targets, metadata `jsonb`,
provenance values, and configuration values are all bound, so a concept titled
`'; DROP TABLE pgokf.concepts; --` is stored as literal text and can never alter
a query.

Where a SQL statement must vary structurally (for example the target column in
`set_config`, or the shared column list in the admin reads), the varying element
is a **fixed identifier chosen in Rust from a closed enum**, never a string
derived from caller input; only the value is bound. Configuration keys and values
are parsed into a typed `ConfigKey`/`ConfigValue` and validated per key before
any statement runs, so an unknown key or wrong-shaped value is rejected with
`22023` rather than reaching SQL.

## The private schema

`pgokf_private` holds internal catalog state (currently the `config` policy row)
that ordinary readers must not see. `USAGE` is granted to `pgokf_admin` only, and
the `config` table has `REVOKE ALL … FROM PUBLIC` with no compensating grant, so
even an admin reaches it only through the `SECURITY DEFINER` config functions —
which authorize the caller first. Readers can observe the effective policy
through `get_config()`, but cannot read or write the table directly.

## Error handling and SQLSTATEs

Failures surface as stable SQLSTATEs so clients can react programmatically rather
than string-matching messages:

| SQLSTATE | Meaning | Example cause |
| -------- | ------- | ------------- |
| `22023` | invalid parameter | relative path, `..` traversal, path outside `allowed_roots`, malformed frontmatter, bad `limit_count`/`max_hops`, unknown/invalid config |
| `42501` | insufficient privilege | missing `pgokf_admin` / `pgokf_reader` membership or `EXECUTE` grant |
| `23505` | unique violation | registering an already-registered canonical path |
| `XX000` | internal error | a broken installation invariant (should not occur in normal use) |

Every error carries the offending bundle-relative path so operators can identify
the object at fault. Server logs should include bundle identity and high-level
failure categories, not full concept bodies. See
[troubleshooting.md](troubleshooting.md) for causes and fixes.
