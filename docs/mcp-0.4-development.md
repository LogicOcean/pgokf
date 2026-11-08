# MCP 0.4 development surface

The finalized 0.4.0 source implements MCP `2026-07-28` alongside
legacy initialization revisions `2024-11-05`, `2025-03-26`, and `2025-06-18`.
Tagging and publication are separate release steps; this is not a blanket conformance claim.

| Surface | Implementation |
| --- | --- |
| Tools | All 14 legacy names plus `traverse_concepts`; strict input/output schemas and structured results |
| Resources | Curated catalog/bundle lists, five templates, canonical metadata and exact stored-byte reads |
| Skills | Optional `io.modelcontextprotocol/skills` extension; complete manifests and direct get |
| Transport | Newline-delimited stdio and authenticated POST JSON HTTP |
| Authentication | Local process authority or custom static bearer tokens; per-request revocation and tenant checks |
| Omitted | Subscriptions, list-change notifications, directory reads, Tasks, MRTR, prompts, completions, sampling, elicitation, SSE, OAuth discovery |

Modern requests carry `params._meta` with
`io.modelcontextprotocol/protocolVersion: "2026-07-28"` and an object-valued
`io.modelcontextprotocol/clientCapabilities`. `server/discover` is available
without initialization. HTTP requests must mirror the version and method in
`MCP-Protocol-Version` and `Mcp-Method`; `Mcp-Name` mirrors a tool name or
resource URI on `tools/call` and `resources/read`. Non-ASCII names use the
protocol's `=?base64?...?=` sentinel. `Accept` must permit JSON; clients should
send `application/json, text/event-stream`. The server produces JSON only.

Legacy stdio clients initialize before calling tools. Legacy HTTP remains
sessionless for compatibility. Successful legacy text payloads retain their
existing shape; structured content is added beside them. Legacy structured
results use an object wrapper (`value`) because legacy protocol schemas require
object roots; modern results carry the native domain value. The Final Skills
extension is modern-only; legacy clients retain `get_skill`. Unknown arguments
and wrong-typed optional filters now fail closed. Execution errors carry stable
domain codes and safe messages, without SQL error chains. Writer tools are
hidden without a usable writer connection. Remote filesystem arguments remain
unavailable.

Resources use these canonical forms:

```text
okf://catalog/manifest
okf://catalog/bundles/{bundle_id}
okf://catalog/bundles/{bundle_id}/concepts/{concept_key}
okf://catalog/bundles/{bundle_id}/documents/{path_key}
skill://pgokf/b/{bundle_id}/c/{concept_key}/{skill_name}/{member_path}
```

Concept and document keys are unpadded base64url of UTF-8 bytes. Bundle IDs are
positive decimal integers. Package path segments use canonical uppercase
percent encoding. Reads reject traversal, encoded separators, noncanonical
forms and unknown members. Tenant identity comes from authentication, never a
URI. A concept's existing `resource` pointer remains opaque metadata: the server
never fetches its target. Concepts are directly addressable but not globally
listed. Skills are listed only by `skills/list`.

Skills discovery returns complete original frontmatter and all owned files,
including explicit `SKILL.md`, with SHA-256 digests and raw byte sizes. Reads use
the existing audited catalog functions and verify package bytes against stored
integrity metadata. Packages exceeding 512 files or 16 MiB, or violating Skill
name/frontmatter invariants, are excluded from extension listing with explicit
refusals on direct get. The compatibility `get_skill` tool remains available.
Discovery does not execute scripts, activate a Skill, grant permissions, or
honor `allowed-tools`.

Pagination uses authenticated keyset cursors. Tokens bind method, process
tenant, stable principal, current role, filters, keyset position, catalog
coordinates and a 15-minute expiry. Catalog changes produce `stale_cursor`;
invalid, expired or out-of-scope tokens produce `invalid_cursor`. Lists and
search execute within repeatable-read snapshots. `OKF_MCP_CURSOR_KEY` optionally
supplies 32 standard-base64-encoded random bytes; otherwise a process-local key
invalidates cursors on restart. Never expose this key or cursor payloads in logs.

Discovery is public-cacheable; role/tenant-dependent lists and reads declare
private cache scope. List TTL is 30 seconds and exact-byte read TTL is five
minutes. These are refetch hints. Producer freshness, catalog generation, source
hash, package hash and per-file SHA-256 remain separate evidence; stale visible
content remains readable. Compatibility Tool reads/searches attach
`_meta["io.logicocean.pgokf/catalogEvidence"]` in both eras. Each entry names
a composite identity, canonical URI, generation, sync hash and freshness. This
is a separate observation after Tool execution, not a shared snapshot claim.
Skill manifest reads and their template use bare `text/markdown`.

The implementation specification and acceptance gates are in
[the MCP specification](https://github.com/LogicOcean/pgokf/blob/main/spec/pgokf-0.4.0-mcp.md). Local contract suites use
real PostgreSQL and authenticated HTTP. Official suites require their own
scenario fixtures; missing diagnostic tools and unsupported optional scenarios
must be reported separately, never counted as passing conformance.

The [frozen applicability matrix](https://github.com/LogicOcean/pgokf/blob/main/spec/mcp-conformance-applicability.json)
preselects Gate A's five unmodified whole scenarios: `tools-list`,
`resources-list`, `sep-2164-resource-not-found`, `dns-rebinding-protection`, and
`http-header-validation`. All must exit zero with zero failed checks. The pinned
Skills enumeration, manifest and directory scenarios are also required.

Gate B runs `cargo run --locked -p pgokf-verifier -- mcp-native . "$PGOKF_PG_CONFIG" "$PGOKF_MCP_BIN" "$EVIDENCE/mcp-native"`
against a real authenticated disposable PG18 HTTP server. The [coverage ledger](https://github.com/LogicOcean/pgokf/blob/main/spec/mcp-native-coverage.json) maps 89 qualified
official check IDs (53 core and 36 Final Skills) to executed native assertions
and requires every live Tool, Resource, template, Skill and member, plus
lifecycle, schema, role/tenant,
cursor, cache and freshness assertions. Both coverage directions and deliberate
coverage/probe-removal controls must pass. Missing setup, skips and empty
selectors fail closed. Full legacy golden and SDK/Inspector checks remain
separate required gates.

The historical full alpha.11 exit-1 result is external, non-gating research,
with all 67 failed checks retained and classified. The frozen runner has no
production fixture mapping or check-level filtering: fixture-coupled/mixed-scope
scenarios cannot certify native pgokf operations. Their underlying applicable
requirements remain mandatory in Gate B and are not declared official passes.
See the [frozen-source citations and exact policy](https://github.com/LogicOcean/pgokf/blob/main/spec/pgokf-0.4.0-mcp.md#111-gate-a-unmodified-official-selected-scenario-evidence).
No blanket official conformance or production-server certification is claimed.
Every designated release gate must pass; no accepted test failures.
Public hermetic tests may use generic loopback literals; private infrastructure
references and credentials remain prohibited.


Opaque catalog concept/document identifiers treat literal percent sequences as
raw UTF-8 data before canonical base64url encoding. URI percent syntax applies
to the actual percent-encoded Skill path segments. Rendering and parsing never
rewrite catalog identities or omit search records to avoid a URI error.

Gate B uses requirement-specific semantic receipts with explicit operation
labels and floors; test totals are diagnostic only. The pinned official wire
schema manifest is [mcp-wire-sources.json](https://github.com/LogicOcean/pgokf/blob/main/spec/mcp-wire-sources.json).
Wire validation and Tool domain schema validation are separate. Skills wire
interfaces are a documented local translation of the pinned official extension
specification, which publishes no JSON Schema. The verifier uses Rust
`jsonschema` and `serde_yaml` dependencies from the workspace lockfile. An external `PGOKF_MCP_SCHEMA_DIR` cache is optional; source
hash verification is mandatory. Cache assertion-removal modes are
`--mutation cache-scope`, `cache-positive`, and `cache-ttl`; each must fail.
