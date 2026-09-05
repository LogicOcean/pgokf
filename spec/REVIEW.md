# Spec review: pgokf Code Catalogue extension (v0.2.1 draft) against pgokf 0.1.14

Reviewed 2026-09-05, before the pg_textsearch backend and the typed-concept work
begin. Sources, all recovered and compared:

| File | Where it lives | Last changed | Note |
| --- | --- | --- | --- |
| `OKF-EXTENSION-SPEC.md` (1,586 lines) | `/datapool/projects/code-catalogue/` only | 2026-08-30 22:16 | The pgokf-facing spec. Never uploaded to Tasker. |
| `archive/code-catalogue-service-SPEC.md` (1,920 lines) | Tasker filer + disk (byte-identical) | 2026-08-30 16:25 | The standalone "Code Catalogue System" service spec (Axum/REST). Archived on 2026-09-05 (see `archive/README.md`); its Appendices B and C remain the source of the CTL1, search, and security clauses the extension spec cites. |
| `ctl1/grammar/ctl1.ebnf`, `ctl1/schemas/`, `ctl1/fixtures/` | Tasker filer + disk (byte-identical) | 2026-08-30 | Normative CTL1 grammar, JSON schemas, and golden fixtures, kept live. The service's OpenAPI document is archived. |

The Tasker project ("Code Catalogue System", five completed tasks) holds no
comments or task-level notes beyond the original task descriptions, so this
directory now carries the complete record. Nothing here is committed yet.

## 1. Verdict

The extension spec is coherent, unusually precise, and grounded in the real
code paths (`ParsedConcept`, `ByteSource`, the sync transaction, the role
model). It can be finalized. What has to change falls into four groups: places
where the 0.1.14 release moved the ground under it, a handful of outright
defects, one core policy conflict that needs your decision, and scope: it
describes far more than the first useful increment needs, and it does not yet
know that a second BM25 backend is coming.

## 2. Must change: contradictions with the shipped 0.1.14 catalog

1. **Backups (new rule since 0.1.14).** Every table and sequence the spec adds
   (§5.3 five projections, §5.5 diagnostics, §8.6 history payloads, §11.1 scan
   tables) is only backed up if it is registered with `pg_extension_config_dump`.
   pgokf now does that through `pgokf_private.register_dump_relations()`,
   which the fresh install calls last and which **every upgrade script must
   call as its last statement**. Add that as a conformance requirement in §5
   and §12 Phase 1. The spec's rebuild functions (§8.7) should also note that
   a restore rehydrates typed projections from the dump, not from `body_text`.
2. **The BM25 seam.** §7.1 and §7.2 are written for native `tsvector` ranking
   only (`ts_rank_cd` mixes, `code_tsv`, `script_tsv`, `reference_tsv`). pgokf
   already dispatches `concept_search` through a `search_backend` seam
   (`native | bm25`), and the plan is to add pg_textsearch as a second BM25
   provider on PostgreSQL 17 and 18. The spec must say: (a) `catalogue_search`
   and the type-specific wrappers follow the same seam; (b) the ranking
   version in the cursor (`catalogue-rank-1`) is per backend, so a BM25 backend
   ships its own ranking version and the formula in §7.2 is the *native*
   formula; (c) type-specific vectors become backend-specific indexes (a
   tsvector under native, a BM25 expression index under pg_textsearch).
3. **RLS and BM25 planning.** 0.1.14 learned that a security-barrier predicate
   with an inline `current_setting()` call breaks ParadeDB's planner, so the
   BM25 path runs in a `SECURITY DEFINER` helper that binds the tenant as a
   parameter. §7.4's "reader access only through `security_barrier` views or
   pinned-search-path `SECURITY DEFINER` functions" is compatible, but the
   spec should state the parameter-binding rule explicitly so the next backend
   does not re-learn it. pg_textsearch is a regular index access method and
   should plan under RLS directly; verify, do not assume.
4. **Companions and adapters.** §6.2/§6.3 correctly keep Git and GitHub out of
   the database. Since 0.1.14 there is a shared companion runtime
   (`pgokf-companion`: watch loop, signals, CLI conventions), a multi-arch
   companions image, and `pgokf-ingest --watch`. The spec should name the
   Git/GitHub adapter as a mode of `pgokf-ingest` (or a sibling binary in that
   image) and require it to record the resolved revision in bundle options
   exactly as §6.3 lists.
5. **MCP is the reference client for §19.2.** Progressive disclosure
   (discovery, activation, resource access) maps one to one onto `pgokf-mcp`
   tools. Make the MCP server the normative implementation of §19.2 and let
   §10 (UI) inherit it, instead of leaving both as prose.
6. **Version numbering.** The document calls itself "extension version 0.2.1";
   pgokf is at 0.1.14 and follows SemVer with a locked public surface. Adding
   tables, types, and roughly twenty functions is a `0.2.0` release under the
   api-stability policy. State the target pgokf release explicitly and keep
   "0.2.1" as the spec/CTL profile version only.

## 3. Must change: defects in the draft

1. **`pgokf.references` is an unusable table name.** `REFERENCES` is a reserved
   word; every DDL and query would need quoting. Rename (`reference_documents`
   or `concept_references`) before anything is built on it (§5.3, §8.6, §17).
2. **Diagnostics use `digest(..., 'sha256')` (§5.5).** That is pgcrypto, which
   pgokf does not depend on and should not add. Use the built-in
   `sha256(bytea)` (`encode(sha256(...), 'hex')`), available on every supported
   major.
3. **`type: Reference` already exists as a free-form type.** The sample bundle
   and any existing catalog use `type: Reference` for ordinary documents with
   no `format`, `author`, or `visibility`. Under `strict` those become sync
   failures; under `warn` they become generic rows plus diagnostics. The
   spec's default (`warn` for pre-existing bundles, `strict` for new API
   bundles) is right; add an explicit sentence that the validator MUST treat a
   legacy Reference without the typed fields as generic, never as an error,
   unless the bundle opts into strict.
4. **`sh` alias conflict.** §3.3 says `sh -> shell`; §16.2 says `sh -> bash`
   only when verified. Keep one rule (§16.2's) and reference it from §3.3.
5. **`pgokf.private_read` needs a mechanism, not a sentence (§7.4).** "Installed
   by a non-login capability-setter role; applications cannot set it" is only
   true if the GUC is registered with `SUSET` context and set transaction-
   locally by a superuser-owned `SECURITY DEFINER` function. `pgokf.tenant` is
   deliberately `USERSET` (a scoping selector, per the 0.1.13 security docs), so
   the spec must not imply the two behave alike. Specify the SUSET GUC plus the
   definer setter.
6. **`links` migration under the no-data-loss rule (§5.6).** The upgrade rules
   forbid `DROP`, `DELETE`, and rewrites; §5.6 uppercases every existing
   `link_relation` and adds a unique index that could fail on legacy
   duplicates. Both are allowed (an `UPDATE` is not a loss), but the spec must
   say what happens when the unique index cannot be built: the recommended
   answer is to coalesce duplicates into one edge with merged `derivation`
   inside the upgrade transaction, and to keep the validation of the index
   in the same transaction so an upgrade never half-applies.
7. **Rendering inside PostgreSQL (§8.4).** Feasible with pgrx, but every render
   holds a backend for the duration and returns up to 25 MiB through a SQL
   result. Keep the SQL function as the semantic definition, and make the
   companions image (or the HTTP layer) the recommended place to run it at
   volume; the spec already hints at this and should say it plainly.

## 4. Decisions taken on 2026-09-05

1. **Tenancy default: option A.** A `require_tenant` durable policy key,
   default `false`, gives hardened deployments the spec's deny-by-default;
   every existing install keeps opt-in tenancy. Ships in its own release
   (every RLS policy is rewritten with `ALTER POLICY`, non-destructively), not
   in the pg_textsearch release.
2. **One product.** The standalone service is archived (`archive/`); the CTL1
   grammar, schemas, and fixtures stay normative under `ctl1/`; the HTTP/UI
   layer is §10 of the extension spec plus the MCP server.
3. **Build order** as proposed in §6: pg_textsearch backend first.

### 4a. pg_textsearch facts established by experiment (v1.4.0 on PostgreSQL 18)

- Ships as one `.deb` per major/arch inside the release archives (PostgreSQL
  17 and 18, amd64 and arm64), so the image installer's pinned-checksum
  mechanism applies unchanged.
- Requires `shared_preload_libraries`; its access method is also named `bm25`,
  so it and ParadeDB cannot coexist in one database. That makes provider
  detection deterministic: whichever extension is installed is the provider.
- **Works under row-level security for a non-owner reader** with the tenant
  policy in place (identical scores as superuser; a foreign tenant sees zero
  rows), with joins and filters in pgokf's real query shape. No
  `SECURITY DEFINER` helper is needed, unlike ParadeDB.
- `to_bm25query(text, index)` needs the schema-qualified index name
  (`'pgokf.<index>'`); `<@>` returns the negated BM25 score as `float8`, every
  row gets a score (non-matches score 0), and the query language is plain
  terms (no web-search operators).

## 4b. Superseded: the tenancy question as originally posed

§7.4 requires "effective tenant is mandatory for application roles; unset
tenant MUST deny rather than retain legacy see-all behavior". That reverses a
documented pgokf contract: tenancy is opt-in, an unset `pgokf.tenant` sees
everything, and the deployment on green-one, the compose stack, and the MCP
reader all rely on it. Two ways to reconcile:

- **A. Policy key.** Add `require_tenant` (default `false`) to the durable
  policy. Hardened multi-tenant deployments set it to `true` and get the
  spec's deny-by-default; single-tenant deployments keep working. Visibility
  (`public`/`internal`/`private`) applies to the five typed projections
  regardless.
- **B. Scope the rule.** Keep see-all for generic concepts and apply deny-by-
  default only to typed catalogue rows. Simpler, but two behaviors in one
  catalog is a support burden.

I recommend A. It preserves every existing install and makes the hardened mode
a one-line policy change, which the compose stack can expose as a variable.

## 5. Needs your decision: what the standalone service spec is now

`code-catalogue/SPEC.md` describes a second product: an Axum REST service with
its own entries, versions, releases, ACLs, API tokens, and an instance-to-
instance sync protocol. The extension spec explicitly says it is "not a second
catalogue" and reuses pgokf's storage, search, provenance, history, and tenancy.
Keeping both means two data models for the same content. Options:

- **Retire the service-level parts** (data model, REST contract, ACL/token
  model, sync protocol) and keep Appendices B and C, the CTL1 grammar, the
  JSON schemas, and the fixtures as the normative CTL1 artifacts that the
  extension spec cites. The HTTP layer becomes §10 of the extension spec plus
  the MCP server.
- **Keep the service as the HTTP/UI front end** over pgokf, rewriting its data
  model sections to point at pgokf's tables and functions.

Either way the OpenAPI document needs regenerating; today it describes the
standalone service's routes, not pgokf's surface.

## 6. Scope: what to build first

The spec is one document, but it is at least five deliverables. Given that the
catalog exists to aggregate knowledge for agents (Claude Code, hermes), the
order that produces value earliest:

1. **pg_textsearch backend** (already agreed): the seam work in §2.2, the image
   and compose changes, 0.1.15.
2. **Skill packages, Script, Reference** (§15 to §18, §5.4 classification, the
   `links` migration): this is what makes agent skills, runbooks-as-code, and
   supporting material first-class, and it exercises the raw-staging pipeline
   every later type depends on.
3. **Code Snippet** (§3): small once the typed-projection machinery exists.
4. **Unified `catalogue_search`, facets, diagnostics retrieval** (§7, §5.5).
5. **Scaffold Template and the CTL1 renderer** (§4, §8.3, §8.4): the largest
   single piece and the one with the least dependency on the others.
6. **Scanners** (§11.1): schema first, engines later; the tables can ship with
   phase 2 so status is representable from day one.

Each step is its own minor release with its own upgrade script and the backup
registration call at the end.

## 7. Confirmed as still accurate

- The `ParsedConcept` and `ByteSource` seams, the `(bundle_id, id)` identity,
  `declared_id` as advisory metadata, and the `filesystem | content` source
  types (§1.3, §6.1, §6.4).
- The role model and the definer-hardening rules (§8.1) match what 0.1.14
  enforces, including pinned `search_path` and `REVOKE ... FROM PUBLIC`.
- Package-hash invalidation (§9) is consistent with the incremental BLAKE3 sync.
- Limits (§11.2) sit above the current GUC ceilings, so existing
  `pgokf.max_*` settings remain the tighter bound as the spec intends.
