# pgokf 0.4.0 MCP surface: Resources, Tools, and Skills — implementation specification

**Status:** accepted for implementation (0.4.0 development line, first point version `0.4.0-dev1`)
**Date:** 2026-09-23
**Protocol target:** MCP `2026-07-28` (Current) plus the Final optional extension `io.modelcontextprotocol/skills` (SEP-2640)
**Audience:** pgokf implementers and reviewers

This document is the in-repository, public-safe specification for the pgokf 0.4.0
MCP work. It resolves every open question from the accepted design analyses,
freezes the wire contracts, and states the exact acceptance criteria. Where an
implementation note in a design analysis conflicts with the finalized official
protocol, the official protocol wins; each such correction is recorded in
§12.

Authoritative inputs (all resolved here):

- MCP `2026-07-28` specification (base, versioning, tools, resources, caching,
  pagination, streamable HTTP, changelog) — normative.
- Final Skills extension specification (SEP-2640, stable `skills.mdx`) — normative
  for the extension surface.
- Accepted pgokf design analyses of 2026-09-22 (integration decision,
  integration architecture, surface code audit, protocol update research) —
  architectural intent.

## 1. Decision summary

pgokf 0.4.0 exposes one catalog and authorization model through three MCP
projections plus a compatibility surface:

1. **Tools** compute, mutate, reconcile, or materialize. All 14 current tool
   names are retained. One typed registry owns every tool's name, minimum role,
   availability, JSON Schema 2020-12 input/output schemas, annotations, and
   handler identity. Results carry validated `structuredContent` beside the
   legacy pretty-printed text block.
2. **Resources** are stable URI-addressed representations of catalog state and
   exact bytes pgokf already retains. `resources/list`, `resources/templates/list`,
   and `resources/read` are implemented. Concepts are addressable but not
   globally enumerated. `resources/read` never dereferences a concept's OKF
   `resource` pointer.
3. **Skills** publish the existing canonical `pgokf.skills` packages through the
   Final `io.modelcontextprotocol/skills` extension: `skills/list`, `skills/get`,
   and exact per-file bytes through `resources/read`. `get_skill` remains as a
   compatibility tool.

Explicitly **not** in 0.4.0: subscriptions (`listChanged`, `subscribe`,
`subscriptions/listen`), `resources/directory/read`, the Tasks extension, MRTR,
sampling, elicitation, roots, prompts, completions, icons, SSE output, OAuth
discovery, JSON-RPC batching. None of these are advertised.

## 2. Architecture

```text
stdio / HTTP transports
        |
   dual-era protocol adapters (legacy 2024-11-05..2025-06-18, modern 2026-07-28)
        |
        +-- ToolAdapter -------- search/traverse/mutate/build/check (14 tools + traverse_concepts)
        +-- ResourceAdapter ---- list descriptors/templates/read exact bytes
        +-- SkillAdapter ------- skills/list, skills/get (manifests only)
                   |
        CatalogReadService / CatalogCommandService  (typed domain values)
                   |
        PolicyService (one authorization decision point for every method)
                   |
        existing pgokf SQL/domain APIs + workspace builder
                   |
        PostgreSQL catalog metadata and stored exact bytes
```

- `CatalogReadService` owns tenant/role-scoped descriptor lookup, canonical URI
  generation/parsing, Skill list/get projection, exact-byte reads through the
  audited domain functions, MIME selection, size bounds, digest verification,
  response metadata, and opaque cursor encode/validate.
- `CatalogCommandService` owns the existing tool actions (mutations, builds,
  checks).
- `PolicyService` is the single point that maps an authenticated
  `PrincipalContext` (principal id, tenant, role, auth mode, request protocol
  version) plus a target operation to allow/refuse. Tool discovery, tool
  invocation, resource listing/reads, and skill list/get all call it. No second
  store and no second authorization model are created; PostgreSQL roles,
  security-definer functions, tenant predicates, and RLS remain the
  defense-in-depth layer.
- Wire adapters never issue ad hoc SQL and never translate a resource read into
  a network fetch, filesystem access outside stored bytes, or any
  content-processing pipeline. pgokf remains a metadata engine over a datalake.

## 3. Protocol and capability matrix

### 3.1 Protocol eras

| Era | Revisions | Negotiation | State |
|---|---|---|---|
| Legacy | `2024-11-05`, `2025-03-26`, `2025-06-18` | `initialize` handshake | stdio: per-process initialization state machine; HTTP: stateless, negotiation-compatible |
| Modern | `2026-07-28` | per-request `_meta` + `server/discover` | fully stateless |

Era classification happens before method dispatch and never from connection
history:

- A request whose method is `server/discover` is modern.
- A request carrying `_meta["io.modelcontextprotocol/protocolVersion"]` is
  modern; the value must be `2026-07-28` or the request fails with
  `UnsupportedProtocolVersion` (`-32022`) with `data` naming the requested and
  supported versions.
- On HTTP, an `MCP-Protocol-Version` header of `2026-07-28` marks modern
  traffic and must agree with the body `_meta` (§3.4).
- `initialize` starts the legacy dialect. A legacy request is any request on a
  legacy-initialized stdio process, or an HTTP request that is not modern by
  the rules above (preserving 0.3.1 HTTP compatibility).

Legacy behavior is separately golden-tested (§9). Modern requests never receive
legacy semantics and vice versa.

### 3.2 Advertised capabilities (modern `server/discover`)

```json
{
  "capabilities": {
    "tools": {"listChanged": false},
    "resources": {},
    "extensions": {
      "io.modelcontextprotocol/skills": {}
    }
  }
}
```

The extensions map lives **inside** `capabilities` (official schema). Nothing
else is advertised. In particular: no `resources.listChanged`, no
`resources.subscribe`, no skills `directoryRead`, no tasks, no prompts.

Legacy `initialize` results advertise `{"tools": {}, "resources": {}}` to
`2025-06-18`; older revisions receive `{"tools": {}}`. The Final Skills
extension requires base `2026-07-28` and is never advertised to legacy clients;
legacy callers retain `get_skill`.

### 3.3 Method inventory

| Method | Era | Min role | Notes |
|---|---|---|---|
| `server/discover` | modern | any authenticated | supported versions, capabilities, extensions, identity |
| `initialize`, `notifications/initialized`, `ping` | legacy | any authenticated | legacy lifecycle only |
| `tools/list` | both | per-tool role filter | deterministic order; role-filtered; availability-filtered |
| `tools/call` | both | per-tool role | 15 tools (14 legacy + `traverse_concepts`) |
| `resources/list` | both | reader | curated: manifest + active bundle descriptors, paginated |
| `resources/templates/list` | both | reader | 5 templates (§5.3) |
| `resources/read` | both | reader | canonical URIs only (§5) |
| `skills/list` | modern | reader | keyset-paginated Skill entries |
| `skills/get` | modern | reader | complete entry by `SKILL.md` URI |

Modern results that are cacheable (`server/discover`, `tools/list`,
`resources/list`, `resources/templates/list`, `resources/read`, `skills/list`,
`skills/get`) carry `resultType: "complete"`, `ttlMs`, and `cacheScope` at the
**top level of `result`** (siblings of the payload, not in `_meta`), as the
official caching contract requires. `server/discover` is identical for every
caller and uses `cacheScope: "public"` with `ttlMs: 3_600_000`; all other
results are tenant/role-dependent and use `cacheScope: "private"`. List TTL:
30_000 ms. Exact immutable byte reads: 300_000 ms. `cacheScope` is identical
across all pages of a paginated list. `ttlMs` is a refetch hint only — never a
freshness or integrity claim. Every modern result also carries
`_meta["io.modelcontextprotocol/serverInfo"]: {"name": "pgokf-mcp", "version":
<crate version>}`.

`server/discover` answers with `supportedVersions: ["2026-07-28"]` (the legacy
revisions are negotiated through the legacy `initialize` path, not per-request),
the capability map of §3.2, and a short `instructions` string.

Era-specific method rules:

- A modern request (`2026-07-28`) for `initialize`, `ping`, or
  `notifications/initialized` is an unknown method: `-32601` (HTTP 404); the
  error message names the supported modern version, as the specification
  directs modern servers to do for legacy clients.
- A modern request missing a required `_meta` field
  (`io.modelcontextprotocol/protocolVersion` or
  `io.modelcontextprotocol/clientCapabilities`) is malformed: `-32602`, HTTP
  400. There is no default version.
- `tools/call` for an unknown tool: `-32602` in the modern era; the legacy era
  keeps the 0.3.1 in-band `isError: true` result (golden-pinned).
- `resources/read`, `skills/get`, and cursor validation failures use `-32602`
  (resource-not-found's legacy `-32002` is not emitted; these methods are new
  in 0.4.0, so no 0.3.1 client depends on the old code).
- `skills/list` and `skills/get` results carry the skill payload under the
  `skills` / `skill` keys respectively, beside `resultType`, `ttlMs`, and
  `cacheScope`.

### 3.4 HTTP transport contract (modern)

- `POST /mcp` only; `GET`/`DELETE` answer 405 with `Allow: POST` (unchanged).
- `Accept` on modern requests must permit `application/json` (or `*/*`);
  otherwise HTTP 400 with `HeaderMismatch` (`-32020`). The specification
  mandates clients send `application/json, text/event-stream`; because no
  pgokf capability produces SSE, a missing `text/event-stream` token is
  tolerated (documented leniency; JSON-only responses remain conformant).
- `MCP-Protocol-Version: 2026-07-28` required on modern requests and must equal
  the body `_meta["io.modelcontextprotocol/protocolVersion"]`; a disagreement
  is HTTP 400 with `HeaderMismatch` (`-32020`).
- `Mcp-Method` required on all modern requests; must equal the body's `method`.
- `Mcp-Name` required on `tools/call` and `resources/read`; must equal the
  body's tool name / resource URI. Non-ASCII values use the official sentinel
  encoding `=?base64?{Base64UTF8}?=` and are decoded before comparison; values
  matching the sentinel pattern are always treated as encoded.
- Header/body disagreement or a missing required modern header: HTTP 400 with
  JSON-RPC `HeaderMismatch` (`-32020`).
- Unknown method on a modern request: HTTP 404 with `-32601`. Legacy requests
  keep the 0.3.1 status mapping (JSON-RPC error with HTTP 200).
- Notifications (no `id`): 202 with an empty body (unchanged).
- JSON-RPC batch arrays: rejected (`-32600`) (unchanged).
- Legacy session headers (`Mcp-Session-Id`, `Last-Event-ID`) are ignored
  (unchanged; sessions do not exist).
- CORS: the 2026-07-28 transport leaves CORS to deployments. pgokf keeps its
  exact-match Origin policy and expands allowed headers to exactly:
  `authorization`, `content-type`, `accept`, `mcp-protocol-version`,
  `mcp-method`, `mcp-name`. Exposed headers: `mcp-protocol-version`.
- Existing controls are preserved: 1 MiB body cap, 15 s body timeout, 32 work
  slots, 60 s request timeout, 50 s statement timeout, exact Origin matching,
  bearer-only auth, per-request token lookup (immediate revocation), exact
  process/token tenant equality, auth-before-body.

### 3.5 JSON-RPC strictness

| Rule | Modern | Legacy |
|---|---|---|
| `"jsonrpc": "2.0"` | required, exact | tolerated if absent; if present must be `"2.0"` |
| request `id` | string or integer, never null/absent-as-request | 0.3.1 three-state behavior (absent = notification, null allowed) preserved |
| batch | rejected | rejected |
| error object | `code`, `message`, optional `data` | same |
| result envelope | `resultType` on every success | 0.3.1 shape (no `resultType`) |
| unknown method | `-32601` | `-32601` |
| unsupported version | `-32022` + `data{requested, supported}` | legacy `initialize` negotiates/falls back as 0.3.1 did |

Error-code allocation: `-32020` (HeaderMismatch) and `-32022`
(UnsupportedProtocolVersion) are used exactly as the specification assigns,
with `-32022`'s `data` as `{"supported": [...], "requested": "..."}`.
`-32021` (MissingRequiredClientCapability) is never emitted because pgokf
requires no client capability. The legacy resource-not-found code `-32002` is
never emitted (§3.3). The 0.3.1 local authorization code `-32001` remains only
on the legacy dialect (its range is legacy-implementation-defined); the modern
dialect uses `-32602` for authorization-gated parameter refusals and in-band
tool errors (`isError: true`) for execution refusals, so no new meaning is
allocated in reserved ranges.

## 4. Tools

### 4.1 Registry

One typed registry (`ToolSpec`) owns, per tool: `name`, `title`, `description`,
`minimum_role`, `availability` (which process connections must exist: reader,
build, writer), `input_schema`, `output_schema` (both JSON Schema 2020-12),
`annotations` (`readOnlyHint`, `destructiveHint`, `idempotentHint`,
`openWorldHint` where accurate), and `handler` identity. `tools/list`,
`tools/call` authorization, host-filesystem-only argument stripping, and
discovery filtering all derive from this registry — the 0.3.1 duplication
across schema arrays, dispatch matches, and the role table is removed. A
registry test proves every advertised tool has exactly one handler, role,
schema pair, and availability predicate, and that the 15-tool inventory is
exact.

### 4.2 Schema contract

- Every input schema is object-rooted with `additionalProperties: false`.
- Ranges, enums, and exclusive selectors are structural: `minimum`/`maximum`,
  `enum`, `minItems`/`maxItems`, and `oneOf` for the exactly-one-of
  `bundle`/`bundle_id` selector.
- `format`/`pattern` bounds mirror existing runtime validation; a wrong-typed
  or unknown argument fails closed. This intentionally tightens 0.3.1, where
  some malformed optional values were silently ignored (documented
  compatibility exception, §8).
- Every tool has an `outputSchema`. `structuredContent` is validated against it
  before sending. Array results stay arrays (2026-07-28 permits any JSON value
  in `structuredContent`).
- Validation uses a bounded JSON Schema 2020-12 evaluator with network and
  file `$ref` resolution disabled.
- `tools/call` success: `content` keeps the exact 0.3.1 pretty-printed JSON
  text block; `structuredContent` carries the same typed value. The legacy text
  is byte-identical to 0.3.1 output for identical catalog state.
- Tool execution failure: JSON-RPC success with `isError: true`,
  `structuredContent: {"code": <stable domain code>, "message": ...}`, and a
  concise text block. Raw SQLSTATE text and `anyhow` chains are never exposed
  (they remain in stderr logs).

### 4.3 Domain error codes (stable)

`invalid_argument`, `not_found`, `permission_denied`, `writer_unavailable`,
`conflict`, `build_refused`, `invalid_cursor`, `stale_cursor`, `invalid_uri`,
`resource_unavailable`, `skill_not_listable`, `internal`.

Cross-tenant, hidden, disabled/retired, and missing objects are
indistinguishable: all surface as `not_found` (tools/skills) or the
resource-not-found class (`resources/read`), with no existence leakage.

### 4.4 The 15 tools

All 14 existing names keep their current accepted inputs (new inputs are
optional only). `concept_neighbors` keeps its exact legacy output shape as the
link-only compatibility alias. One new tool:

**`traverse_concepts`** (reader): typed traversal over both graph surfaces.

```json
{
  "bundle_id": 42,
  "concept_id": "src/main",
  "graph": "relationships",
  "direction": "outbound",
  "relation_types": ["code:calls"],
  "max_hops": 2,
  "limit": 500
}
```

- `graph: "relationships"` (default) → `pgokf.concept_relationship_neighbors`
  (direction, relation-type filter, current relationship generation, per-node
  effective freshness).
- `graph: "links"` → the Markdown-link traversal behind `concept_neighbors`.
- Output: array of `{bundle_id, concept_id, hops, path, relation_type, title, direction, catalog_generation,
  freshness: {state, reasons}, uri}` where `uri` is the canonical concept
  resource URI (§5). `relation_type` is null on the links graph.
- `max_hops` is capped by `pgokf.max_graph_hops`; `limit` defaults 500, max
  10_000 (mirroring the SQL cap).

### 4.5 Availability

Write/admin tools are advertised only when the process holds a writer
connection; `build_workspace_plugin` requires the build connection;
`list_bundles` runs on the reader connection (0.3.1 policy preserved). Stdio
with no `--writer-url` no longer advertises write/admin tools. Calling an
unadvertised tool fails with `writer_unavailable`/`permission_denied` as
applicable.

## 5. Resources

### 5.1 Canonical URI grammar (frozen)

```text
okf://catalog/manifest
okf://catalog/bundles/{bundle_id}
okf://catalog/bundles/{bundle_id}/concepts/{concept_key}
okf://catalog/bundles/{bundle_id}/documents/{path_key}
skill://pgokf/b/{bundle_id}/c/{concept_key}/{skill_name}/SKILL.md
skill://pgokf/b/{bundle_id}/c/{concept_key}/{skill_name}/{member_path}
```

- `bundle_id`: decimal integer `1..2^63-1`, no leading zeros, no sign, no
  alternate encodings.
- `concept_key`: unpadded base64url (RFC 4648 §5) of the UTF-8 concept id.
  Decoded length ≤ 512 bytes.
- `path_key`: unpadded base64url of the UTF-8 catalog document path.
- Catalog concept/document keys encode opaque raw UTF-8 bytes losslessly.
  Literal `%20`, `%2f`, `%25`, incomplete `%`, and repeated `%` are data inside
  these keys, never URI escapes. No percent decoding or residual-escape check
  runs on opaque keys. Decoded path structure still rejects traversal, empty
  components, backslashes and controls; base64url canonicality and size limits
  remain mandatory. Skill concept keys use the same opaque representation.
- `skill_name`: percent-encoded segment equal to the package's
  `frontmatter.name` and to the final segment of the package root.
- `member_path`: package-relative POSIX path below the package root, one
  percent-encoded segment per path component (`SKILL.md` is the explicit
  terminal form, not a `member_path`).
- Percent encoding: uppercase hex; only unreserved characters
  (`A-Z a-z 0-9 - . _ ~`) emitted literally. Decode exactly once.
- Total URI length ≤ 2048 bytes; each raw segment ≤ 1024 bytes.

Parsing produces typed fields, re-renders canonically, and compares the
re-rendered URI byte-for-byte with the input; any mismatch is `invalid_uri`.
Rejected: traversal (`..`, `.`), absolute paths, empty segments, encoded
separators in percent-encoded URI segments (`%2f`, `%2F`, `%5c`), backslashes,
NUL (`%00`), residual escapes in percent-encoded Skill segments, double decoding,
noncanonical base64url/percent forms, overlong input, unknown package members
(member must appear in the canonical package manifest), and any scheme or
authority other than the frozen ones. `file://` URIs, host paths, SQL
identifiers, object-store keys, and the OKF concept `resource` pointer are
never accepted as read targets.

The URI never contains a tenant. Tenant and role come from the authenticated
request context; URI possession is addressing, not authority.

### 5.2 Resource classes and reads

| URI class | Content | MIME |
|---|---|---|
| catalog manifest | JSON: schema id, server/extension versions, `pgokf.capabilities()` evidence, catalog coordinates | `application/vnd.pgokf.catalog+json` |
| bundle descriptor | JSON: `pgokf.bundle.v1` (id, name, source type, state, file count, sync hash, catalog generation, freshness) | `application/vnd.pgokf.bundle+json` |
| concept metadata | JSON: `pgokf.concept.v1` (identity, path, type, title, description, tags, status/trust, opaque `resource` pointer as metadata, body text, modified_at, file hash, freshness, `document_uri` when retained) | `application/vnd.pgokf.concept+json` |
| exact document | exact retained bytes from `pgokf.concept_source`; never synthesized from `body_text` | `text/markdown; charset=utf-8` or `application/octet-stream` by sniffing |
| `SKILL.md` | exact stored bytes | `text/markdown` |
| skill member (script/reference/asset) | exact stored bytes via audited readers | stored media type / language inference |

Text content is returned as UTF-8 `text`; binary as base64 `blob`. Every read
records the same audit event as its underlying domain operation
(`get_concept_source`, `get_skill`, `get_script`, `get_reference`). Reads of
stale-but-visible objects succeed with freshness annotations in `_meta`.
Disabled/retired bundles are not listed or read (uniform `not_found` class).
Cross-tenant, hidden, and missing URIs are externally indistinguishable.

### 5.3 Listing

- `resources/list`: the catalog manifest plus one descriptor per visible active
  bundle, keyset-paginated by `bundle_id`. Concepts, documents, package
  members, skills, and build artifacts are **not** enumerated.
- `resources/templates/list` advertises five RFC 6570 templates:
  `okf://catalog/bundles/{bundle_id}`,
  `okf://catalog/bundles/{bundle_id}/concepts/{concept_key}`,
  `okf://catalog/bundles/{bundle_id}/documents/{path_key}`,
  `skill://pgokf/b/{bundle_id}/c/{concept_key}/{skill_name}/SKILL.md`,
  `skill://pgokf/b/{bundle_id}/c/{concept_key}/{skill_name}/{+member_path}`.
- `concept_search`, `find_similar`, `traverse_concepts`, `concept_neighbors`,
  `get_concept`, and `get_skill` results include canonical resource links
  (in `structuredContent` `uri` fields and, on the modern adapter,
  `resource_link` content items) so discovery flows into direct reads.
- `skills/list` is the only Skill registry; skills are not duplicated into the
  global resource list.

### 5.4 Resource `_meta`

Resource descriptors and read results carry pgokf evidence under the owned
namespace `io.logicocean.pgokf/`:

```json
{
  "io.logicocean.pgokf/bundleId": 42,
  "io.logicocean.pgokf/conceptId": "skills/deploy/SKILL",
  "io.logicocean.pgokf/fileHash": "<catalog blake3 identity>",
  "io.logicocean.pgokf/packageHash": "<skill package closure identity>",
  "io.logicocean.pgokf/syncHash": "<bundle sync identity>",
  "io.logicocean.pgokf/catalogGeneration": 17,
  "io.logicocean.pgokf/freshness": {"state": "fresh", "reasons": []}
}
```

These identities stay separate: MCP `ttlMs` (cache hint), per-file
`sha256:`+size (byte integrity), `package_hash` (package closure), `file_hash`
(catalog source identity), `sync_hash` (bundle sync identity),
`catalog_generation` (snapshot coordinate), producer provenance (opaque
evidence), and pgokf freshness (producer-attested state). None substitutes for
another.

## 6. Skills extension

Advertisement: core `resources: {}` plus `extensions:
{"io.modelcontextprotocol/skills": {}}` (no `directoryRead`).

### 6.1 Entry shape

```json
{
  "uri": "skill://pgokf/b/42/c/c2tpbGxzL2RlcGxveS9TS0lMTA/deploy/SKILL.md",
  "frontmatter": { "...verbatim agent_skill frontmatter..." : "..." },
  "resources": [
    {"uri": "skill://pgokf/b/42/c/.../deploy/SKILL.md",
     "digest": "sha256:<64 lowercase hex>", "size": 1234},
    {"uri": "skill://pgokf/b/42/c/.../deploy/scripts/check.sh",
     "digest": "sha256:...", "size": 321}
  ]
}
```

- `frontmatter` is the complete stored `agent_skill` object, verbatim.
- The manifest is always complete (never `"dynamic"`), includes `SKILL.md`
  itself and every owned script/reference/asset exactly once, and is atomic —
  never split across pages.
- The path segment before `SKILL.md` equals `frontmatter.name`. Packages
  violating the Agent Skills name invariant are not advertised through the
  extension (they remain available through `get_skill`); `skills/get` on their
  URIs answers `skill_not_listable`.
- Packages exceeding the extension's interoperability limits (512 files,
  16 MiB) are likewise not advertised (`skill_not_listable` on `skills/get`),
  with the limit named in the message. No file bodies are loaded for list/get.
- pgokf `_meta` on entries carries `packageHash`, `fileHash`,
  `catalogGeneration`, and `freshness` (§5.4). Frontmatter and file bytes are
  never mutated.

### 6.2 Methods

- `skills/list`: paginated visible Skill entries in `(bundle_id, concept_id)`
  keyset order. Supports empty, partial, multi-page, and duplicate-name
  catalogs honestly.
- `skills/get`: resolves a canonical `SKILL.md` URI (including a known skill
  omitted from a partial listing) and returns the same complete entry shape.
  Unknown/cross-tenant URIs are indistinguishable (`not_found` class).
- `resources/read` returns exact `SKILL.md`, script, reference, and asset
  bytes through the existing audited readers, with byte size and
  `sha256:` digest matching the manifest exactly.

Skill discovery never activates, executes, installs, or grants permissions.
`allowed-tools` is not honored, scripts are not executed, and nothing is
injected into model context. pgokf returns bytes, metadata, digests, and
provenance only.

### 6.3 Storage gap closure

`pgokf.skills` gains two columns in the `0.3.1 → 0.4.0-dev1` upgrade edge:
`skill_md_sha256 text` (lowercase hex SHA-256 of the exact `SKILL.md` bytes)
and `skill_md_byte_size bigint`, populated at ingestion and backfilled
idempotently in the edge. `package_hash` and `file_hash` are not substitutes.
The columns are additive; existing function signatures do not change
(API-stability surface is extended, not altered).

## 7. Pagination and cursors

All MCP-native lists (`concept_search` continuation, `resources/list`,
`skills/list`) use opaque authenticated keyset cursors. Offsets are never
used and totals are never fabricated.

Cursor payload (canonical JSON, then HMAC-SHA-256, then unpadded base64url;
total token ≤ 1024 bytes):

```json
{
  "v": 1,
  "m": "skills/list",
  "t": "<tenant or empty>",
  "p": "<role>/<token digest>",
  "f": "<sha256 of normalized filter set>",
  "k": ["skills/deploy/SKILL", 42],
  "g": "<sha256 of ordered tenant-visible catalog coordinates>",
  "exp": 1790123456
}
```

- `v` format version (unknown versions rejected); `m` binding method; `t`
  tenant; `p` principal scope = role plus the non-reversible stable principal
  (the bearer token's SHA-256 digest; `local` on stdio); `f` filter hash; `k`
  keyset position; `g` digest of the ordered tenant-visible bundle IDs, generations, state and sync hashes at page creation; `exp` expiry
  (default 15 minutes from issuance).
- The MAC key comes from `OKF_MCP_CURSOR_KEY` (32 bytes, base64) or, when
  unset, a per-process random key (cursors then expire at restart, which is
  documented).
- Rejected indistinguishably as `invalid_cursor`: tampering, wrong method,
  wrong tenant, wrong principal/role, filter drift, expiry, malformed payload,
  unknown version. A cursor whose `g` no longer equals the current catalog
  generation is rejected as `stale_cursor` (retry from the first page); this
  is the documented substitute for snapshot retention and guarantees no
  duplicate/omitted rows within a generation. On the wire: for `skills/list`
  and `resources/list` both map to JSON-RPC `-32602` with the stable domain
  code in `error.data.code`; for `concept_search` (a Tool) they surface as
  `isError: true` with the same domain codes in `structuredContent` (§4.2).
- `concept_search`: the opaque cursor translates to the existing SQL
  `(rank, bundle_id, concept_id)` keyset; the SQL JSON tuple is never exposed.
  The next cursor rides in result `_meta["io.logicocean.pgokf/nextCursor"]` so
  `structuredContent` and the legacy text stay the same result array.
- Page limits are applied before any exact file bytes are loaded.

## 8. Compatibility behavior

1. All 14 tool names and their currently accepted inputs keep working; new
   inputs are optional.
2. The legacy pretty-printed text block is byte-identical for identical
   catalog state; `structuredContent` is added beside it.
3. Legacy `initialize` negotiation and fallback behave as 0.3.1 (golden-tested);
   stdio additionally enforces initialization ordering (§3.1). HTTP stays
   sessionless; ordering cannot be tracked there and is documented.
4. `get_skill` keeps its exact 0.3.1 payload shape, derived from the same
   canonical record the extension projects.
5. Unknown/extra tool arguments and wrong-typed optionals now fail closed
   instead of being ignored — the one intentional tightening, called out in
   the changelog and tool descriptions.
6. Old clients see only the surfaces their era expresses; no task results, no
   modern envelopes on legacy traffic.
7. Generated workspace plugin trees are not Skills and do not appear in
   `skills/list`.
8. Write tools disappear from discovery when the process cannot execute them
   (no writer connection) — advertised capability equals executable capability.

## 9. Phase 0 golden fixtures

Before any behavior change, wire fixtures are captured from the 0.3.1
implementation and committed under `tests/mcp-golden/`:

- `tools/list` for each role (reader/builder/writer/admin over HTTP) and for
  stdio/local, byte-exact.
- `tools/call` for all 14 tools with canonical inputs against a seeded real
  PostgreSQL 18 fixture catalog, over stdio and over authenticated HTTP for
  each permitted role, plus refused-role cases.
- Legacy `initialize` negotiation for each supported revision and one unknown
  revision (fallback), plus `ping`, notifications (202), batch rejection, and
  malformed JSON-RPC cases.

Volatile fields (timestamps, catalog generations, hashes derived from fixture
randomness) are normalized by a committed, reviewed normalizer before
comparison; the normalizer is part of the fixture suite and mutation-tested.
The 0.4.0 implementation must keep these fixtures green under the comparison
rules of §8 (legacy text byte-identity; additive fields allowed where §8
permits).

## 10. Migration and version plan

- Workspace/crate version `0.3.1` → `0.4.0-dev1`; `pgokf.control`
  `default_version` moves with it (point-version convention `MAJOR.MINOR.PATCH-devN`).
- New upgrade edge `crates/extension/sql/pgokf--0.3.1--0.4.0-dev1.sql`:
  idempotent statements only (`CREATE OR REPLACE`, guarded `DO` blocks,
  membership adoption before re-creation), no destructive statements, ending
  with `SELECT pgokf_private.register_dump_relations();`. It carries the
  `pgokf.skills` digest columns (§6.3) and any later 0.4.0 schema delta.
- Later point versions advance by ordinary `devN -> devN+1` edges; a reviewed
  point is never rewritten in place. `pgokf--0.4.0-dev1--0.4.0-dev2.sql`
  reconciles field search-function drift (retired 0.2.0 overloads and
  11-argument `bm25_hits`, current-helper install, fresh ACLs/comments/initial
  privileges) behind named fail-closed identity guards, `RESTRICT` drops only
  (see “Reconciling field drift” in `docs/api-stability.md`).
  `pgokf--0.4.0-dev2--0.4.0-dev3.sql` completes that catalog's reconciliation
  by restoring seven superseded `pgokf_web` comments; its appended
  `pgokf_web.users`/`pgokf_web.sessions` column order is the one declared
  catalog exception, kept safe by named-column access only (see “Declared
  catalog exceptions” in `docs/api-stability.md`).
- No committed `pgokf--0.4.0-dev1.sql` fresh-install script (dev point
  versions treat it as a build product); `META.json` keeps pointing at the
  last published release `0.3.1` and its committed install script — the same
  policy the 0.3.0 dev cycle used.
- Distribution artifacts (RPM spec, Dockerfiles, compose `.env.example`,
  issue template, Homebrew formula) stay pinned at `0.3.1`; no tag, no
  publication tuple, no image manifest changes. `packaging/resolve-release.sh`
  and `pgokf-verifier compatibility-source` accept exactly the current dev version
  as eligible source (publication remains impossible without an immutable
  matching tag, which is never created for dev points).
- Release guards that hardcode `0.3.1` as “current” are updated so that
  `0.3.1` remains protected as the last published release (digests, tuples,
  history) while the dev line advances; `tests/workflow-contexts.json` is
  regenerated only if a workflow file changes.
- the Rust `parity` command point-version routes gain the `0.4.0-devN`
  chain; fresh-install vs upgrade parity from `0.3.1` (and from `0.2.0`
  through the full chain) must hold object-for-object, and a reconciled field
  catalog must equal a fresh install in every member-catalog object except the
  declared column-order exception.
- `v0.3.0` history and all `0.3.1` recovery/release-integrity controls are
  preserved.
- Public API stability: new SQL objects extend the pinned
  `PUBLIC_FUNCTIONS`/`PUBLIC_TYPES`/`CATALOG_TABLES` lists with comments;
  existing signatures do not change.

## 11. Acceptance criteria

0.4.0-dev1 may claim the surface of §3 only when all of the following are green
on the final committed tip:

1. `cargo fmt --all -- --check`.
2. Strict Clippy: extension PG18 and non-extension workspace, all targets,
   `-D warnings`.
3. `scripts/test-workspace.sh --quiet` (all host suites).
4. In-database PG18 `cargo pgrx test` suites, plain and with the existing
   provider-preload requirement.
5. the Rust `parity` command: fresh install and every upgrade route,
   including `0.3.1 → 0.4.0-dev1`, inventory-identical in real PostgreSQL 18.
6. Golden legacy compatibility suite (§9) green.
7. Authenticated HTTP black-box tests for every role and tenant boundary,
   including: malformed JSON-RPC, unsupported versions/capabilities, invalid
   headers and header/body mismatch, cross-tenant and cross-resource probes,
   role downgrade/change, token revocation, unavailable runtime capabilities,
   cursor tampering/expiry/cross-method reuse, URI double-decoding,
   noncanonical encodings and traversal, binary/text byte fidelity, digest/size
   drift, partial Skills pages, missing/unauthorized indistinguishability, and
   schema validation of every real success/failure shape.
8. Two independent mandatory gates against an authenticated disposable PG18
   HTTP instance: Gate A (unmodified official selected scenarios) and Gate B
   (complete native advertised-surface assertions), as defined below. Official
   Skills extension scenarios are additionally mandatory. Every designated
   command must exit zero with zero failed checks; no expected-failure baseline.
9. Inspector and available official SDK interoperability checks; any leg that
   cannot run locally is disclosed as unsupported, never replaced by a claim.
10. URI parser property/fuzz tests (round-trip canonicalization over ASCII,
    Unicode, slash- and percent-containing, and maximum-length inputs; hostile
    corpus from §5.1).
11. Exact-byte and digest tests: `skills/list`/`skills/get` manifests equal
    every `resources/read` result byte-for-byte; `get_skill` parity.
12. `cargo audit --deny warnings`, `cargo deny check`, notices regeneration
    parity.
13. Repository workflow/release guard suites and their mutation tests.
14. `git diff --check e40a7dedb2dcf0a3846ee36db85eaab3690593f6...HEAD`, a
    public-repo hygiene scan (no credentials, hostnames, LAN IPs, SSH/rsync or
    deploy tooling, machine-local paths), and a clean `git status --short`.

Zero failing tests; zero waived failures. Mock-only or string-pinning tests do
not count as production proof of wire behavior; wire contracts are pinned to
real response types and exercised through real routing and database bindings.

### 11.1 Gate A: unmodified official selected-scenario evidence

Freeze `@modelcontextprotocol/conformance@0.2.0-alpha.11`, gitHead
`c321dd32035556e6769d3724a8ee97d87c3faaac`, integrity
`sha512-imPK9tx5gQsL6ZKQq4MrsyDYfSaIwpRmX6+ogjbeAXs9LGvxkBxWcY7KcS7TvwaBk/ZiVWl6b/naF4q83UwDRA==`.
Verify the installed executable and requirements against the integrity-checked
registry archive before execution. Preselect these whole scenarios using the
[applicability manifest](mcp-conformance-applicability.json), and compare its
capabilities to live `server/discover` before running:

| Whole scenario | Recorded passing checks | Disclosed warnings |
| --- | ---: | ---: |
| `tools-list` | 3 | 0 |
| `resources-list` | 2 | 0 |
| `sep-2164-resource-not-found` | 3 | 1 |
| `dns-rebinding-protection` | 2 | 0 |
| `http-header-validation` | 14 | 0 |

Run each with `node <frozen-package>/dist/index.js server --url <scratch-url>
--scenario <name> --spec-version 2026-07-28 --output-dir <external-evidence>
--verbose`. Each command must exit zero with zero failed checks and nonzero
successful checks. Retain raw output, including the Resource-not-found SHOULD
warning about omitted `data.uri`. Authentication may be supplied without changing
requests, identifiers, protocol headers, assertions or results.

Run official `sep-2640-skills-enumeration`, `sep-2640-skills-manifest`, and
`sep-2640-skills-directory` from the unmodified pinned Skills source
`7169291ec0b68eb370fddcd9947313ab0d5e4156`, with the same arguments and `--force`
for extension timeline selection. These have 37 successful checks, zero failures,
two metadata warnings, and six optional directory skips; every command exits
zero. Directory omission is also asserted negatively by Gate B.

The historical full alpha.11 diagnostic (50 scenarios, exit 1, 100 successes,
67 failures, three warnings, one info, six skips) remains external research
with its original check-by-check classification. It is **not a release gate**,
not a pgokf test pass, and is not rerun as a required final gate. No failed
check is hidden, relabeled, baselined or declared passed.

The frozen [CLI source](https://github.com/modelcontextprotocol/conformance/blob/c321dd32035556e6769d3724a8ee97d87c3faaac/src/index.ts)
provides whole-scenario selection but no production fixture mapping, adapter,
capability-derived selector, or check-level filtering. The
[requirements loader](https://github.com/modelcontextprotocol/conformance/blob/c321dd32035556e6769d3724a8ee97d87c3faaac/src/requirements.ts)
accepts only bundled requirement revisions. The
[Tool scenarios](https://github.com/modelcontextprotocol/conformance/blob/c321dd32035556e6769d3724a8ee97d87c3faaac/src/scenarios/server/tools.ts)
and [Resource scenarios](https://github.com/modelcontextprotocol/conformance/blob/c321dd32035556e6769d3724a8ee97d87c3faaac/src/scenarios/server/resources.ts)
hardcode diagnostic `test_*` names and `test://*` URIs. Mixed-scope
[stateless](https://github.com/modelcontextprotocol/conformance/blob/c321dd32035556e6769d3724a8ee97d87c3faaac/src/scenarios/server/stateless.ts)
and [caching](https://github.com/modelcontextprotocol/conformance/blob/c321dd32035556e6769d3724a8ee97d87c3faaac/src/scenarios/server/caching.ts)
scenarios include unadvertised features. Their applicable protocol requirements
remain mandatory in Gate B; these official scenarios are not declared passed.
No facade or diagnostic production API is introduced.

### 11.2 Gate B: native advertised-surface verification

Run `cargo run --locked -p pgokf-verifier -- mcp-native . "$PGOKF_PG_CONFIG" "$PGOKF_MCP_BIN" "$EVIDENCE/mcp-native"`
with the current MCP executable and an installed current PG18 extension. Schema
validation uses the Rust dependencies pinned by the workspace lockfile. Missing dependencies, skipped setup, zero-test selectors, failed
assertions, missing coverage, and surviving coverage mutations fail the gate.
The probe uses the real router, HTTP bearer authentication and disposable
PostgreSQL; mocks cannot supply evidence.

The [machine-readable requirement ledger](mcp-native-coverage.json) joins
89 distinct official scenario/check identities (53 frozen core and 36 Final
Skills) to executed native assertions, including fixture-coupled requirements. These are traceability IDs,
not a claim that their official scenarios executed or passed. Repeated official
check instances share their qualified identity. The separately disclosed
`sep-2164-data-uri` SHOULD warning is not claimed passed. The native pagination
fixture deliberately includes duplicate Skill display names, addressed by distinct
canonical URIs; the Skills name-uniqueness SHOULD is not a required native
assertion. Global Skill Resource metadata SHOULD omissions remain disclosed.
Additional project requirements mandate every listed native test and every
per-item event. Requirement credit comes only from the ledger's 159 named
semantic labels with explicit positive count floors, scoped to the qualified
requirement and operation. Each receipt is emitted by its successful predicate;
whole-test assertion totals cannot satisfy any requirement. Deleting, zeroing,
or lowering any required receipt below its floor fails that exact row. Unknown
or stale semantic labels fail the reverse join. Test/assertion event totals are
diagnostics and must not be presented as semantic coverage counts.

The [wire source manifest](mcp-wire-sources.json) pins the complete official
MCP JSON Schemas by immutable commit and SHA-256, including all three legacy
revisions. Missing or mismatched sources fail setup. `PGOKF_MCP_SCHEMA_DIR`
may point to an external verified cache; otherwise the exact pinned sources
are fetched and hash-checked. Core 2026-07-28 schema SHA-256 is
`ef70b61f99b6d2e5e3b46863822eab08dff6a45bedc7a08914e0e5b133f40203`.
The Skills extension publishes TypeScript interfaces in its pinned normative
Markdown, not a JSON Schema artifact; the Rust wire verifier explicitly translates those
advertised interfaces and composes them with the official core definitions.
This local extension projection is not described as an official schema.

Validation covers method requests, response envelopes and concrete results,
including all Tool content variants and error text. Complete results are bound
to the concrete method result definition: the generated response union alone
also accepts a permissive MRTR branch and is insufficient. ID correlation,
metadata key grammar and unadvertised result-type refusal supplement the
upstream structural schema. Tool domain input/output/error 2020-12 validation
has separate receipts and cannot substitute for protocol wire validation.
Live HTTP response-corruption controls cover descriptors, content variants,
required fields, schema roots, IDs, errors, metadata, Resources and Skills.
Cache controls independently corrupt scope, type, sign and exact TTL, while
three probe-removal modes omit the cache assertion families during real calls.

The gate discovers all Tools, paginates curated Resources and Skills, reads
all owned members, and matches all five template classes to concrete reads.
Its disposable catalog currently yields 15 Tools, 56 listed Resources, five
templates, six Resource classes, 55 Skills and 111 owned members. Eleven methods
include eight modern methods and three legacy lifecycle methods. These counts
are fixture expectations, never a substitute for the live join.

Each Tool must have successful and typed-error calls, independent JSON Schema
2020-12 input/output/error validation, and structured/text compatibility.
Every Tool also executes in each reader/builder/writer/admin and writer-present/
absent cell (120 cells), checking availability, omission and refusal. Resources
and Skills require exact bytes, MIME, integrity, metadata, canonical addressing,
pagination, missing/cross-tenant behavior and freshness. Lifecycle, modern
headers/metadata, IDs/errors/batches/notifications, authenticated cursor scope,
revocation/downgrade, generation changes, cache fields and unadvertised methods
have mandatory executed assertions. Legacy golden fixtures remain a separate
required gate for exact compatibility.

Both coverage directions are mandatory: each live item and required applicable
protocol identity has nonzero execution evidence; each execution fixture still
belongs to discovery. Eighteen deliberate join mutations must be rejected,
including new/deleted items and removed requirements. Additionally run the gate
with each `--mutation` value (`tool-calls`, `resource-reads`, `lifecycle`,
`headers`, `schemas`, `role-tenant`): each removes real probes or schema
validation and must fail for missing executed coverage. Mutation failures are
expected negative controls within an overall exit-zero mutation campaign, not
accepted product failures.

### 11.3 Permitted claims

Formal MCP tier approval concerns
[SDKs](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/a99bdd15ea9ff96bf4d758aac907bbb27e9329dc/docs/community/sdk-tiers.mdx).
Neither a selected-scenario pass nor a native project gate grants production
server certification. Only after every designated gate passes on the final tip
may the following claim be used:

> The tested pgokf revision passes the five named unmodified official core
> scenarios and the pinned official Skills scenarios with zero failed checks.
> Every advertised pgokf operation passes the project-owned native PG18/HTTP
> surface gate. The retained full alpha.11 diagnostic remains non-passing
> because the runner requires diagnostic fixtures and has no production
> mapping or check-selection facility. No blanket official MCP conformance
> or server certification is claimed. All designated release gates pass.

## 12. Corrections to the design analyses

This section records every place the finalized official protocol required a
deviation from the 2026-09-22 design analyses (architectural intent preserved).
Each entry names the conflict and the resolution. Verified against the official
`2026-07-28` specification pages and the stable Skills extension
(`ext-skills/specification/stable/skills.mdx`) on 2026-09-23.

1. **Cache fields placement.** The analyses variously described modern caching
   as “result types and caching fields” without pinning placement. Official
   resolution: `resultType`, `ttlMs`, and `cacheScope` are top-level members of
   `result` (siblings of the payload), never inside `_meta`; `ttlMs` and
   `cacheScope` are REQUIRED on `server/discover`, `tools/list`,
   `resources/list`, `resources/templates/list`, `resources/read`, and (per the
   extension) `skills/list`/`skills/get`. pgokf implements exactly that (§3.3).
2. **Extensions map placement.** Official resolution: extension advertisements
   live at `capabilities.extensions`, inside the capabilities object (§3.2).
3. **Unknown tool call.** The 0.3.1 behavior (and the audit's error-channel
   description) reports an unknown tool as an in-band `isError: true` result.
   Official `2026-07-28`: unknown tool / malformed call is a JSON-RPC error
   `-32602`. Resolution: era-split — modern uses `-32602`; the legacy dialect
   keeps the golden-pinned 0.3.1 behavior (§3.3).
4. **Removed methods.** `initialize`, `ping`, `notifications/initialized`,
   `logging/setLevel`, and `resources/subscribe`/`unsubscribe` are REMOVED from
   the modern wire. Official resolution: a modern-version request for them is
   `-32601` (HTTP 404), and the error SHOULD name supported versions. pgokf
   keeps them exclusively on the legacy dialect (§3.3) — this is the dual-era
   intent of the analyses, now pinned to the exact codes.
5. **Resource/skill not-found.** Official `2026-07-28`: `-32602` (the legacy
   `-32002` is reserved-but-not-emitted). pgokf's Resource/Skill methods are
   new, so `-32602` is used in both eras with no compatibility loss (§3.3).
6. **Missing per-request metadata.** There is no default protocol version:
   missing `_meta["io.modelcontextprotocol/protocolVersion"]` or
   `clientCapabilities` on a modern request is `-32602` + HTTP 400 (§3.3). The
   only version default the specification permits (missing
   `MCP-Protocol-Version` header treated as `2025-03-26` for pre-2025-06-18
   clients) is subsumed by pgokf's legacy HTTP path, which already served those
   clients statelessly.
7. **HTTP status mapping.** Official: header missing/mismatch → 400 +
   `-32020`; unsupported version → 400 + `-32022`; unknown method → 404 +
   `-32601`. The 0.3.1 mapping (all JSON-RPC answers at HTTP 200) is preserved
   for legacy traffic only (§3.4).
8. **`Accept` header.** Official clients MUST send `application/json,
   text/event-stream`. pgokf validates that JSON is acceptable but tolerates a
   missing SSE token because it implements no SSE-producing feature (documented
   leniency, §3.4). If the official conformance suite proves to require strict
   rejection, this entry will be revisited and the CHANGELOG updated.
9. **CORS.** The 2026-07-28 transport specifies no CORS behavior; pgokf's
   exact-match Origin policy is a deployment control, documented as such (§3.4).
10. **Skills result keys.** Official: `skills/list` → `skills`, `skills/get` →
    `skill` (singular), both with required `resultType`/`ttlMs`/`cacheScope`;
    entry shape `{uri, frontmatter, resources}` with `digest: "sha256:<64
    lowercase hex>"` and raw-byte `size` (§6.1). The analyses' example shapes
    match; the placement of cache fields follows item 1.
11. **Skill `_meta` namespace.** The extension reserves
    `io.modelcontextprotocol.skills/` (dotted) for extension-defined metadata.
    pgokf evidence therefore rides exclusively in the owned
    `io.logicocean.pgokf/` namespace (§5.4).
12. **Pagination field names.** Official: request param `cursor`, result field
    `nextCursor`; an empty-string cursor is valid and is not end-of-results;
    invalid cursors are `-32602`. pgokf's opaque-cursor rejection maps
    `invalid_cursor`/`stale_cursor` to `-32602` with the domain code in
    `data` (§7).

13. **Modern error IDs.** The official `JSONRPCErrorResponse.id` is optional
    and, when present, is a string or integer. Modern uncorrelatable errors omit
    it; legacy errors preserve `id: null`. This is distinct from request ID
    validation and is tested at transport boundaries.
14. **Multi-bundle cursor coordinates.** A single bundle generation cannot
    identify a multi-bundle catalog snapshot. Cursor `g` therefore authenticates
    a SHA-256 digest of ordered tenant-visible bundle coordinates, including
    enabled/retired state. Issuance and page queries share a repeatable-read
    transaction. This refines the originally illustrative integer field.
15. **Traversal coordinates.** The new traversal output carries the requested
    effective `direction` and each reached bundle's `catalog_generation` beside
    the edge type and freshness. Both graph paths use composite identity steps.

16. **Legacy structured result object roots.** Official legacy schemas require
    an object-root `outputSchema` and object `structuredContent`; modern permits
    arbitrary JSON. Official SDK 2.0.0 testing rejected native legacy arrays.
    The legacy adapter therefore wraps every domain value as `{"value": ...}`
    with a closed object-root output schema. The text block remains byte-identical
    and modern structured results remain native domain values. This supersedes
    earlier statements that both eras return identical structured JSON.
17. **Skills protocol minimum.** The Final extension targets base `2026-07-28`
    or later. Legacy `initialize` never advertises it and legacy `skills/*`
    requests are unknown methods; `get_skill` remains the compatible fallback.

## 13. Documentation obligations

Before 0.4.0 is claimed anywhere: the exact capability matrix (§3), the URI
rules (§5), cache semantics (§3.3), cursor behavior (§7), the intentional
schema tightening (§8.5), and the unsupported-by-design list (§1) are
published in user-facing docs (`docs/`, crate READMEs), and `CHANGELOG.md`
carries the `[Unreleased]` 0.4.0-dev1 entries. Documentation must not imply
MCP OAuth conformance; the static bearer mode is documented as a custom,
deployment-specific mechanism.

### Continuation clarifications

Compatibility Tool reads/searches (`concept_search`, `find_similar`,
`concept_neighbors`, `get_concept`, `get_skill`) carry a typed catalog-evidence
array in result `_meta["io.logicocean.pgokf/catalogEvidence"]` in both eras.
Each entry names a composite identity, canonical URI, catalog generation,
sync hash and effective freshness. This is a separate observation immediately
after the operation, not a claim that the earlier result bytes and freshness
were read in one database snapshot. Ambiguous legacy neighbor IDs retain all
visible matching composite identities. Legacy text bytes remain unchanged.
Traversal carries its freshness in each structured node; Resources and Skills
retain their existing namespaced evidence. Cache TTL never attests freshness.

The Skills manifest read and template use bare `text/markdown`, following the
extension's MIME SHOULD; this does not transform stored bytes or digests.

Public hygiene permits generic loopback literals in hermetic tests and test
fixtures. It prohibits private infrastructure addresses/hostnames, credentials,
machine-local paths and operational deployment tooling in new tracked content.
Frozen compatibility fixtures are preserved rather than rewritten to satisfy a
literal ban on all IP text.
