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

Three cluster-wide `NOLOGIN` roles are created idempotently at extension install
(`sql/bootstrap.sql`), forming a strict least-privilege hierarchy
**`pgokf_reader` < `pgokf_writer` < `pgokf_admin`**. You grant one of them to a
real login user; nobody logs in *as* them.

| Role | Capabilities |
| ---- | ------------ |
| `pgokf_reader` | `USAGE` on schema `pgokf`; `SELECT` on the projection tables (including `concept_source`); `EXECUTE` on read paths: `concept_search`, `concept_neighbors`, `list_bundles`, `bundle_info`, `get_config`, `get_concept_source`. |
| `pgokf_writer` | Inherits `pgokf_reader` (it is `GRANT`ed the reader role), plus `USAGE` on schema `pgokf` and `EXECUTE` on the **ingestion** mutators: `register_bundle`, `refresh_bundle`, `unregister_bundle`. It cannot change configuration, write exports, or read `pgokf_private`. This is the intended account for an automated ingestion pipeline and the mountless content-ingestion API (`register_bundle_content`), which streams object-store bytes into the catalog with no filesystem mount. |
| `pgokf_admin` | Inherits `pgokf_writer` (and thus `pgokf_reader`), plus `USAGE` on `pgokf_private` and `EXECUTE` on the admin-only surface: configuration (`set_config`, `reset_config`) and the file-writing exports (`export_parquet`, `export_sources`). |

The hierarchy is established with two role grants — `GRANT pgokf_reader TO
pgokf_writer` and `GRANT pgokf_writer TO pgokf_admin` — so each tier inherits
everything below it: a writer can also search, and an admin can also ingest and
search. Each grant is idempotent (`GRANT` is a no-op when the membership already
exists), so re-installing the extension never disturbs an existing hierarchy.

`PUBLIC` is stripped: `REVOKE ALL ON SCHEMA pgokf FROM PUBLIC` and
`REVOKE ALL ON SCHEMA pgokf_private FROM PUBLIC` run first, and every function
does `REVOKE ALL … FROM PUBLIC` before granting `EXECUTE` to exactly the role
that needs it. The one deliberate exception is `pgokf.version()`, which exposes
only the crate version string and is left executable by everyone.

Because the tiers are separable, an analytics user can be granted read-only
search access (`pgokf_reader`) without ever being able to ingest a bundle; an
ingestion pipeline can be granted `pgokf_writer` to register, refresh, and
unregister bundles without ever being able to rewrite configuration, write files
back to the server, or read the private configuration schema; and only
`pgokf_admin` reaches that last, most dangerous surface.

## Defense in depth: grants plus an in-function check

Authorization is enforced at **two independent layers**:

1. **SQL `EXECUTE` grants** — a caller without the grant is rejected by
   PostgreSQL before the function body runs (`42501 permission denied for
   function …`).
2. **An in-function role check** — every entry point calls
   `security::authorize_current_user(Operation::{Register|Ingest|Search}, …)`,
   which evaluates `pg_has_role` and raises `42501` when membership is missing.
   The three operations map onto the three tiers: `Search` requires
   `pgokf_reader`, `Ingest` (register/refresh/unregister) requires
   `pgokf_writer`, and `Register` — the admin-only surface: configuration and
   the file-writing exports — requires `pgokf_admin`. Each check accepts the
   required role *or any higher tier*, so a higher tier is authorized for a
   lower operation exactly as the role-grant hierarchy implies.

The two layers are intentionally redundant: a future grant mistake, a `SECURITY
DEFINER` boundary, or a superuser path cannot silently bypass the policy, because
the role check runs regardless of how execution was reached.

Observed on a live cluster (three login users granted `pgokf_reader`,
`pgokf_writer`, and `pgokf_admin` respectively):

```text
-- reader invoking an ingestion mutator (grant layer rejects first):
ERROR:  42501: permission denied for function register_bundle
-- writer invoking an admin-only mutator:
ERROR:  42501: permission denied for function set_config
-- writer invoking a file-writing export:
ERROR:  42501: permission denied for function export_parquet
-- writer CAN ingest, and admin CAN do everything, by inheritance.
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
restricted to the ingest tier `pgokf_writer` (which `pgokf_admin` inherits).
Configuring `allowed_roots` is the recommended hardening step for any
multi-tenant or shared cluster.

`allowed_roots` entries are themselves validated as absolute, traversal-free
paths when set, so a malformed root cannot be stored.

## Server-side file writes (`export_parquet`)

Every function described so far only *reads* the filesystem. `export_parquet`
(`crates/extension/src/catalog/export.rs`) is the **single exception**: it writes
one Apache Parquet file per catalog table for a bundle into a server-side
directory. Because a file *write* from inside the backend is strictly more
dangerous than a read, it is guarded at least as tightly as registration:

- **Admin-only.** It is `SECURITY DEFINER`, `GRANT`ed `EXECUTE` to `pgokf_admin`
  alone, and calls `authorize_current_user(Operation::Register, …)` — the
  admin-tier gate — in its body, so neither a reader nor a writer can be granted
  it accidentally nor reach it through the definer boundary. No
  reader-executable or writer-executable path writes files.
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

## Source retrieval and reconstruction

The opt-in `store_source` tier (`crates/extension/src/catalog/source.rs`) adds
two retrieval functions with deliberately different authorization, because they
have deliberately different disclosure and side-effect profiles:

- **`get_concept_source` — reader-level, no filesystem side effect.** It returns
  a concept's stored `bytea` **to the client** and writes nothing to disk, so it
  has no path-security surface at all. Its disclosure is exactly the concept's
  own source — the same content the reader-visible `body_text` is derived from —
  so it is `GRANT`ed `EXECUTE` to `pgokf_reader` and calls
  `authorize_current_user(Operation::Search, …)`, the same gate as
  `concept_search`. It adds no privilege beyond read access to the catalog. When
  no source was stored (the bundle was synced with `store_source` off) it raises
  `22023` rather than inventing bytes.
- **`export_sources` — admin-only, a server-side file write.** Reconstructing a
  bundle on disk *writes files* from inside the backend, so it is guarded exactly
  like `export_parquet`: `SECURITY DEFINER`, `GRANT`ed to `pgokf_admin` alone,
  and `authorize_current_user(Operation::Register, …)` in its body. It **reuses**
  `export.rs`'s `validate_dest_dir` (absolute, NUL-free, traversal-free,
  canonical, `allowed_roots`-contained when configured, existing, writable) and
  `create_export_file` (`O_NOFOLLOW`, so a symlink planted at a target file name
  is refused with `22023` rather than redirecting the write) — the security logic
  is shared, not duplicated. Each stored source path is additionally re-validated
  as a plain bundle-relative path before it is joined under `dest_dir`, and every
  written file is verified against the concept's recorded BLAKE3 `file_hash`, so
  a corrupted stored source aborts the reconstruction (`22023`) instead of being
  written out silently. The same **residual risk** as `export_parquet` applies
  when no `allowed_roots` are configured, and is mitigated the same way.

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
| `42501` | insufficient privilege | missing `pgokf_reader` / `pgokf_writer` / `pgokf_admin` membership or `EXECUTE` grant |
| `23505` | unique violation | registering an already-registered canonical path |
| `XX000` | internal error | a broken installation invariant (should not occur in normal use) |

Every error carries the offending bundle-relative path so operators can identify
the object at fault. Server logs should include bundle identity and high-level
failure categories, not full concept bodies. See
[troubleshooting.md](troubleshooting.md) for causes and fixes.
