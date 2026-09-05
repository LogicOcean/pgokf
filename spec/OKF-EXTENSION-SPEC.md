# pgokf Code Catalogue Extension Specification

**Status:** Proposed implementation specification  
**Target:** `pgokf` PostgreSQL extension  
**OKF compatibility:** Open Knowledge Format v0.2  
**Template protocol:** CTL1  
**Document version:** 0.2.1

## 1. Overview

This specification adds five domain concept types to pgokf:

- `type: Code Snippet`
- `type: Scaffold Template`
- `type: Skill`
- `type: Script`
- `type: Reference`

Standalone Markdown forms are ordinary OKF v0.2 concepts, not a fork of OKF and not a second catalogue. `SKILL.md` and raw package resources are source artifacts classified before generic parsing and materialized as virtual ordinary OKF concepts. OKF intentionally does not centrally register concept types, requires consumers to tolerate unknown types, and preserves producer-defined frontmatter. The existing pgokf parser already represents `type` as an unrestricted string and preserves unknown frontmatter in `ParsedConcept.metadata`; `ParsedConcept` nevertheless requires both `type` and `title`, so `SKILL.md` MUST be transformed before that parser is invoked. Therefore existing pgokf installations can ingest these documents as generic concepts immediately. This extension adds validation, exact payload projection, optimized search, retrieval, and deterministic CTL1 rendering.

The extension SHALL preserve the existing generic projection:

- identity, hierarchy, title, description, type, tags, resource, body search text;
- provenance, links, trust, lifecycle, history, tenancy, and bundle synchronization;
- round-trippable source bytes when `store_source=true`;
- distribution through filesystem or content bundle synchronization.

The extension SHALL add type-specific projections rather than overloading the core `pgokf.concepts` row with many nullable columns. A concept remains addressable by the existing composite identity `(bundle_id, id)`, where `id` is the normalized bundle-relative path without `.md`. A declared frontmatter `id` remains advisory metadata (`declared_id`) and MUST NOT replace the path-derived identity.

### 1.1 Goals

1. Store code snippets, scaffold templates, Agent Skills, executable helpers, and reference material in portable, human-readable source formats.
2. Accept the same concepts from local filesystems, ordinary Git repositories, GitHub, object stores, APIs, or application databases.
3. Search all concept types together by text, tags, type, provenance, and lifecycle fields, with type-specific filters such as language, while retaining pgokf's tenant isolation.
4. Retrieve exact snippet code, Script bytes, Reference bytes, and CTL1 source, not the lossy plain-text search projection.
5. Render a validated scaffold deterministically into a canonical ZIP from typed JSON parameters.
6. Keep the UI replaceable and outside the PostgreSQL extension.

### 1.2 Non-goals

- Registering these type names in the upstream OKF specification.
- Executing snippets, scripts, skills, or generated projects inside PostgreSQL. Script execution, if offered by a trusted external client, is gated by the controls in §20.4.
- Providing shell-, SQL-, HTML-, YAML-, or programming-language-specific escaping.
- Letting PostgreSQL clone arbitrary network repositories directly.
- Treating GitHub as a privileged source of truth.
- Storing secrets in CTL1 parameters.
- Mutating application source trees from a reader-level SQL function.

### 1.3 Compatibility with the current codebase

This specification is grounded in the current implementation:

- `crates/okf-parser/src/model.rs` exposes generic `ParsedConcept` fields and preserves unknown frontmatter in `metadata`.
- `crates/extension/src/catalog/schema.rs` stores generic concepts and metadata, with GIN indexes on tags, search vectors, and metadata JSONB and RLS keyed by `tenant_id`.
- `crates/extension/src/catalog/search.rs` supports ranked full-text search, exact concept type, all-of tags, provenance status/trust filters, keyset cursors, and limits from 1 through 500.
- `crates/extension/src/catalog/sync.rs` provides one atomic, incremental projection pipeline over a `ByteSource`, currently backed by filesystem and in-memory content sources. Skill-package ingestion extends file discovery to recognized sibling resources while preserving that transaction boundary.
- `crates/extension/src/catalog/source.rs` optionally stores exact source bytes in `pgokf.concept_source`; source storage defaults off and therefore cannot be the only backing store for rendering.
- `docs/sql-api.md` exposes `register_bundle`, `register_bundle_content`, `refresh_bundle`, `concept_search`, source retrieval, and the role model used below.

The current schema constrains `pgokf.bundles.source_type` to `filesystem` or `content`. Consequently Git and GitHub adapters in this specification are companion-side adapters that materialize either a filesystem tree or `(path, bytes)` content. They MUST NOT claim a native database `source_type` until a schema migration implements it.

## 2. Normative language and terminology

The key words **MUST**, **MUST NOT**, **REQUIRED**, **SHALL**, **SHALL NOT**, **SHOULD**, **SHOULD NOT**, and **MAY** are normative.

- **Generic projection:** the existing `pgokf.concepts` and `pgokf.concept_metadata` rows.
- **Type projection:** a new row containing validated fields and exact payload for one supported concept type.
- **Source adapter:** a process that obtains files and supplies them to existing pgokf sync entry points.
- **Snippet payload:** the exact contents of the canonical fenced code block under `# Schema`.
- **Scaffold source:** CTL1 text and output-file descriptors parsed from `# Computation`.
- **Render:** validation plus deterministic CTL1 evaluation and archive construction. It never executes output.
- **CTL1:** the template language and archive profile defined by the standalone Code Catalogue specification, Appendices B and C. Those appendices take precedence over conflicting earlier examples or prose.
- **Staged source file:** confined normalized bundle-relative path plus exact bytes, byte length, lowercase BLAKE3 digest, source kind, and snapshot identity, before any Markdown parse.
- **Source-file class:** exactly one of `OkfDocument`, `SkillManifest`, `SkillScript`, `SkillReference`, `SkillAsset`, or `Ignored`.
- **Virtual concept:** a generic and typed projection materialized from a non-OKF source artifact without rewriting that artifact.
- **CTL1 protocol version:** integer `1`. `template_engine: CTL1` names the language; `template_protocol_version: 1` selects the normative grammar. Renderer software SemVer is separate.

## 3. Concept type: `Code Snippet`

### 3.1 Canonical document

````markdown
---
type: Code Snippet
title: Retry an async operation with exponential backoff
description: Bounded retry helper with jitter and cancellation support.
language: python
tags: [retry, async, resilience]
visibility: internal
author: human:alice
source:
  kind: github
  url: https://github.com/acme/platform/blob/2dd9c90/lib/retry.py
  revision: 2dd9c90
resource: https://github.com/acme/platform/blob/2dd9c90/lib/retry.py
---

# Schema

```python
async def retry(operation, *, attempts=5):
    ...
```

# Examples

```python
result = await retry(lambda: client.fetch())
```
````

The outer four-backtick fence is only documentation framing; the actual concept uses ordinary Markdown fences.

### 3.2 Frontmatter

The generic OKF fields retain their OKF meanings. Extension-specific fields are:

| Field | Type | Required | Rules |
| --- | --- | --- | --- |
| `type` | string | yes | Exact value `Code Snippet`. |
| `title` | string | yes for this extension | Non-empty display name. Upstream OKF permits omission, but typed projection requires it. |
| `description` | string | recommended | One sentence suitable for search previews. |
| `language` | string | yes | Lowercase canonical language identifier. See §3.3. |
| `tags` | array of strings | recommended | Stored in `pgokf.concepts.tags`; tags are case-sensitive at storage and SHOULD be lowercase slugs. |
| `visibility` | string | yes | One of `public`, `internal`, `private`; defaults to `internal` only in API-generated documents. File-authored documents MUST be explicit. |
| `author` | string or mapping | yes | Prefer the OKF actor convention (`human:<id>`, `team:<id>`, `agent:<id>`, `process:<id>`). A mapping MAY add display metadata. |
| `source` | string or mapping | recommended | Origin of this exact snippet. Mapping form is canonical. This is distinct from OKF `sources`, which records derivation/provenance. |
| `resource` | URI or JSON value | optional | Canonical underlying asset using normal OKF semantics. |
| `license` | string | recommended for imported public code | SPDX identifier or `LicenseRef-*`. |
| `filename` | string | optional | Suggested filename, never an automatic write target. |

Canonical `source` mappings:

```yaml
source:
  kind: github              # github | git | filesystem | database | api | generated
  url: https://github.com/acme/repo/blob/<commit>/src/file.rs
  repository: https://github.com/acme/repo.git
  revision: <immutable commit SHA>
  path: src/file.rs
  start_line: 10
  end_line: 42
```

For a local origin:

```yaml
source:
  kind: filesystem
  path: snippets/retry.py
```

For database/content insertion:

```yaml
source:
  kind: database
  system: code-catalogue
  record_id: snip_01J...
  revision: 7
```

A mutable branch URL MAY be recorded for navigation, but reproducible imports SHOULD also record an immutable revision. Credentials, access tokens, DSNs, and private headers MUST NOT appear in `source`.

`author` describes who authored or published the concept. `source` describes where this payload came from. OKF `sources` describes materials from which claims were derived. Implementations MUST NOT collapse these fields.

### 3.3 Language identifiers

`language` MUST match `^[a-z][a-z0-9_+.#-]{0,63}$`. The implementation SHALL maintain an alias normalization table at ingestion/API boundaries, for example `py → python`, `js → javascript`, `ts → typescript`, `rs → rust`, `sh → shell`, and `postgresql → sql`. Stored metadata and search filters use the canonical value. Unknown but syntactically valid languages are accepted.

The canonical fenced block SHOULD use the same language identifier. A mismatch between frontmatter `language` and the canonical block's info string is a typed validation error. An empty fence info string MAY inherit the frontmatter language.

### 3.4 Body structure

The canonical snippet body has exactly one first-level `# Schema` section and zero or one first-level `# Examples` section.

#### `# Schema`

- MUST contain exactly one canonical fenced code block.
- That block is the snippet payload.
- Prose before or after the block MAY explain inputs, outputs, assumptions, dependencies, or safety constraints.
- Nested fences in code are represented using a longer outer Markdown fence in the normal Markdown manner.
- The payload is preserved byte-for-byte after UTF-8 validation except that the enclosing Markdown fence is removed.
- Empty payloads are invalid.

The term `# Schema` follows the OKF conventional heading. For code it describes the reusable artifact contract and carries the canonical implementation.

#### `# Examples`

- MAY contain prose and any number of fenced blocks.
- Example blocks are indexed separately at lower weight than the canonical payload.
- Example blocks are not returned as the snippet payload and are never executed.

Additional first-level sections such as `# Dependencies`, `# Notes`, `# Security`, and `# Sources` MAY appear. The typed validator MUST ignore unknown sections after preserving them in source.

### 3.5 Validation

A `Code Snippet` is valid for typed projection only if:

1. the generic OKF document parses;
2. `type`, `title`, `language`, `visibility`, and `author` satisfy this section;
3. the `# Schema` section and canonical fence are unambiguous;
4. the document, fence info string, and payload are valid UTF-8;
5. the payload and document are within pgokf's configured file limits;
6. source metadata contains no recognized secret-bearing keys (`token`, `password`, `secret`, `authorization`, `private_key`);
7. if `license` is present, it is a string and not empty.

Typed strictness is controlled by a new bundle option:

```json
{"validate_code_catalogue": "strict"}
```

Allowed values are `strict`, `warn`, and `off`.

- `strict`: a malformed supported type aborts the bundle sync atomically with SQLSTATE `22023`.
- `warn`: retain the generic concept, omit its type projection, and append a structured sync warning.
- `off`: perform only generic OKF ingestion.

The default SHOULD be `strict` for new API-created code bundles and `warn` for pre-existing generic bundles during migration.

## 4. Concept type: `Scaffold Template`

### 4.1 Canonical document

````markdown
---
type: Scaffold Template
title: Rust command-line application
description: A minimal Cargo CLI with optional GitHub Actions.
tags: [rust, cli, cargo]
visibility: internal
author: team:developer-experience
template_engine: CTL1
template_protocol_version: 1
output_format: zip
parameters:
  schema_version: 1
  variables:
    - name: project_name
      type: string
      required: true
      pattern: '^[a-z][a-z0-9-]+$'
    - name: use_ci
      type: boolean
      default: true
  limits:
    max_output_files: 100
    max_total_bytes: 10485760
---

# Schema

- `project_name`: Cargo package and output directory name.
- `use_ci`: include the GitHub Actions workflow.

# Computation

```ctl1 file="Cargo.toml" executable=false
[package]
name = "{{ project_name }}"
version = "0.1.0"
edition = "2024"
```

```ctl1 file="src/main.rs" executable=false
fn main() {
    println!("Hello from {{ project_name }}");
}
```

```ctl1 file=".github/workflows/ci.yml" when="use_ci" executable=false
name: CI
on: [push]
```
````

### 4.2 Frontmatter

| Field | Type | Required | Rules |
| --- | --- | --- | --- |
| `type` | string | yes | Exact value `Scaffold Template`. |
| `title` | string | yes for this extension | Non-empty. |
| `description` | string | recommended | Search/display summary. |
| `tags` | array of strings | recommended | Generic OKF tags. |
| `visibility` | string | yes | `public`, `internal`, or `private`. |
| `author` | string or mapping | yes | Same rules as Code Snippet. |
| `source` | string or mapping | optional | Same origin mapping as Code Snippet. |
| `template_engine` | string | yes | Exact value `CTL1`; matching is case-sensitive. |
| `template_protocol_version` | integer | yes | Exact value `1` for this specification; unsupported values are never downgraded. |
| `parameters` | mapping | yes | CTL1 variable schema described in §4.4. |
| `output_format` | string | yes | `zip` for extension version 0.2.1. `directory`, if seen in a migrated document, is descriptive only and cannot be returned by `render_scaffold_zip`. |
| `builtin` | boolean | optional | Distribution hint only; does not confer trust. Defaults false. |
| `renderer_min_version` | string | optional | SemVer floor for compatible renderer. |

Trust MUST derive from normal OKF provenance/verification fields and local policy. Neither `builtin: true` nor a GitHub organization name is a trust proof.

### 4.3 Body structure and multi-file envelope

A scaffold body MUST contain one first-level `# Schema` section followed by one first-level `# Computation` section.

#### `# Schema`

This section explains the public parameter contract. It MAY include a JSON or YAML representation of `parameters`, but frontmatter is authoritative. If a machine-readable schema fence is present, the projector SHOULD compare it to frontmatter and report drift.

#### `# Computation`

The section contains one or more fenced `ctl1` blocks. Each block is one output-file descriptor. Parsers MUST read the original Markdown fence boundaries and body bytes, not a lossy Markdown AST rendering. The typed grammar is:

```ebnf
info-string = "ctl1", white, attribute, { white, attribute }, [ whitespace ] ;
white       = whitespace, { whitespace } ;
whitespace  = " " | "\t" ;
attribute   = file-attr | when-attr | executable-attr | endings-attr ;
file-attr   = "file=", json-string ;
when-attr   = "when=", json-string ;
executable-attr = "executable=", ( "true" | "false" ) ;
endings-attr = "line_endings=", json-string ;
json-string = '"', { unescaped | escape }, '"' ;
unescaped   = ? any Unicode scalar except U+0000..U+001F, '"', or "\" ? ;
escape      = "\", ( '"' | "\" | "/" | "b" | "f" | "n" | "r" | "t"
              | "u", hex, hex, hex, hex ) ;
hex         = "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7"
            | "8" | "9" | "A" | "B" | "C" | "D" | "E" | "F"
            | "a" | "b" | "c" | "d" | "e" | "f" ;
```

`file`, `when`, and `line_endings` values therefore MUST be quoted JSON strings with RFC 8259 escapes. `executable` MUST be an unquoted JSON boolean. `file` is REQUIRED and non-empty after JSON decoding. `when` is optional and, when present, MUST be non-empty and syntactically valid; use `when="false"` to intentionally omit a descriptor. `line_endings` is optional and its decoded value MUST be `lf`, `crlf`, or `inherit`; it defaults to `inherit`. `executable` defaults to `false`.

Attributes MAY appear in any order. Unknown or duplicate attributes, single-quoted values, quoted booleans, unquoted string values, missing whitespace between attributes, and trailing non-whitespace tokens are invalid. Fence indentation follows CommonMark; either backtick or tilde fences are accepted, but the closing fence delimiter and length MUST conform to CommonMark. The body is exact UTF-8 CTL1 source. An empty file body is valid. Fence order has no effect on archive order; final files are ordered by unsigned UTF-8 output-path bytes. The canonical examples above (`file="Cargo.toml" executable=false`) conform to this grammar.

This Markdown envelope is the OKF transport representation of standalone scaffold file descriptors. It intentionally does not rely on arbitrary non-Markdown files being discovered by the current pgokf sync pipeline. A future protocol version MAY add referenced template files, but it must define byte discovery, hashing, and portability before doing so.

Extension version 0.2.1 supports text scaffold files only. Binary pass-through files from the standalone catalogue are deferred because embedding opaque binary payloads into OKF Markdown would weaken readability and exact-byte handling. A future extension MAY add content-addressed binary resources; it MUST preserve the standalone rule that binary files cannot be CTL templates and are copied byte-for-byte.

### 4.4 Parameter schema

`parameters.schema_version` MUST be integer `1`. `parameters.variables` MUST be an ordered array; JSON/YAML object insertion order MUST NOT determine dependency evaluation.

Each declaration has:

| Key | Meaning |
| --- | --- |
| `name` | Identifier matching `^[A-Za-z][A-Za-z0-9_]*$`; names beginning with `@` are reserved. |
| `type` | `string`, `integer`, `number`, `boolean`, or homogeneous scalar `array`. Arbitrary objects and nested arrays are forbidden. |
| `required` | Boolean. |
| `default` | Literal of the exact declared type. |
| `default_from` | Dependency expression supported by the CTL1 schema. |
| `computed` | Deterministic computed value supported by the CTL1 schema. Computed values are not accepted as caller input. |
| `enum` | Exact-type allowed values. |
| `pattern` | RE2-compatible pattern for strings. |
| `min_length`, `max_length` | String bounds. |
| `minimum`, `maximum` | Numeric bounds. |
| `items` | Scalar item schema for arrays. |
| `min_items`, `max_items`, `unique_items` | Array bounds and uniqueness. |

Unknown input properties are rejected. Inputs are never coerced. Defaults and computed dependencies are resolved as a dependency graph; cycles and references to absent declarations are template-ingestion errors. Simultaneously ready nodes are evaluated in ascending UTF-8 variable-name byte order.

String inputs are NFC-normalized, then CRLF and lone CR are converted to LF before validation and hashing. Numbers must be finite interoperable JSON values; NaN, infinities, negative zero, and out-of-range values are rejected. Array uniqueness uses RFC 8785 canonical item bytes. Secret variable declarations are unsupported and MUST be rejected.

The optional `parameters.limits` mapping MAY reduce, but never increase, server ceilings for output file count, bytes per output file, total bytes before and after line-ending conversion, path bytes, path segments, nesting depth, and render time.

### 4.5 CTL1 language

The implementation SHALL use the CTL1 grammar and evaluation rules from the standalone Code Catalogue specification. In summary:

- interpolation: `{{ reference | transform }}`;
- comments: `{{! ... }}`;
- blocks: `#if`, `#unless`, and `#each`, each with optional `else`;
- conditions: boolean reference, optional unary `not`, typed `==`/`!=`, and typed `contains(array, literal)`;
- iteration locals: `this`, zero-based `@index`, `@first`, and `@last`;
- transforms: `trim`, `lower`, `upper`, `snake_case`, `kebab_case`, `pascal_case`, `camel_case`, and `kebab_to_snake`;
- whitespace trim markers: `{{~` and `~}}` remove only contiguous ASCII space, tab, CR, and LF adjacent to the directive;
- literal delimiters: `{{{{` emits `{{` and `}}}}` emits `}}`;
- one substitution pass only;
- maximum block nesting depth 16.

There is no implicit truthiness, coercion, property access, `and`, `or`, arithmetic, regex execution, assignment, function chaining, or language-specific escaping. Type mismatch is an error, not `false`. Unknown variables and transforms make the scaffold invalid at ingestion. Arrays cannot be interpolated directly. `this` and `@...` locals are valid only inside `each`.

Scalar serialization is deterministic: strings unchanged and unescaped, booleans lowercase, integers base-10 without leading zero, and numbers serialized per RFC 8785. Non-ASCII content is preserved and not transliterated.

CTL1/1 normalization and digest rules are normative here:

- Input strings are NFC-normalized and CRLF/lone CR become LF before schema validation. Template source is UTF-8 validated but otherwise byte-preserved; neither template bodies nor rendered bodies are implicitly NFC-normalized. Output paths alone are NFC-normalized after CTL1 evaluation. Final body line-ending conversion is the last content transform.
- `template_manifest` source ranges are zero-based, half-open UTF-8 byte offsets `[start,end)` into `template_source`; they cover only each original fence body, including its final newline if present, and never count Unicode scalar values or UTF-16 code units.
- Missing input means apply a default or report required-input failure. Explicit JSON `null` is invalid because CTL1/1 declares no nullable type. Empty strings/arrays are valid only when their declared bounds permit them.
- The canonical input digest is lowercase hexadecimal SHA-256 of RFC 8785 canonical JSON for `{"template_protocol_version":1,"variables":<normalized supplied/defaulted public values>,"target_platform":<value>,"line_endings":<value>,"output":<zip|manifest|json>}`. Computed values MUST appear in a separate `computed_variables_sha256` digest in the manifest and MUST NOT leak secret values (secrets are unsupported in CTL1/1).
- Each output entry has lowercase-hex SHA-256 over its exact post-line-ending bytes. `output_sha256` covers the exact returned bytes for `zip` or `json`. For `manifest`, it covers RFC 8785 canonical JSON of the manifest with the top-level `output_sha256` member omitted; the returned manifest then carries that digest, avoiding self-reference.
- If every `when` evaluates false, rendering succeeds with an empty file manifest. ZIP output is the canonical empty ZIP (EOCD only, no comment); JSON/manifest output uses an empty files array.

Ingestion records protocol version 1. A renderer MUST reject an unsupported `template_protocol_version` or a `renderer_min_version` above its software SemVer with SQLSTATE `0A000`; it MUST NOT silently fallback or downgrade. Protocol version, renderer compatibility version, grammar/transform fixture version, and ranking/parser versions participate in cache identity. Any semantic behavior change requires a new protocol version or a new renderer version where semantics are unchanged.

### 4.6 Output path safety

For every rendered file:

1. evaluate `when`; omit the file if false;
2. render `file` once using only permitted scalar string/integer variables and transforms;
3. NFC-normalize the result;
4. validate the path before allocating archive state;
5. render the body once;
6. apply final line-ending conversion;
7. enforce pre- and post-conversion limits;
8. hash the output;
9. reject all collisions before constructing any response.

Output paths MUST be UTF-8, POSIX-relative, and free of a leading slash, backslash, NUL, empty segment, `.` segment, or `..` segment. No segment may exceed 240 UTF-8 bytes and the path may not exceed 1,024 UTF-8 bytes, subject to stricter server limits.

`target_platform` is one of `portable`, `linux`, `macos`, or `windows`, default `portable`. Portable mode applies every platform's restrictions. The renderer rejects Windows device names, colons where forbidden, trailing spaces/dots, and Windows case-insensitive collisions. It rejects macOS NFC/case-fold collisions. It also rejects exact-byte collisions. Generated paths never become host filesystem paths inside the SQL function.

### 4.7 Canonical ZIP

`render_scaffold_zip` returns `application/zip` bytes with this deterministic profile:

- ZIP method 0 (`STORE`), no compression variability;
- no directory entries, duplicate decoded names, comments, encryption, data descriptors, ZIP64, or extra fields;
- UTF-8 member names and general-purpose bit 11 only;
- Unix/3.0 `version made by` (`0x031e`) and version-needed 1.0;
- fixed DOS timestamp `1980-01-01 00:00:00` for every local and central directory entry, independent of source/history/wall-clock time;
- external mode `0100755 << 16` for executable files and `0100644 << 16` otherwise;
- internal attributes zero;
- IEEE CRC-32;
- records ordered by unsigned UTF-8 member-name bytes.

The renderer MUST first build and validate a complete in-memory/spooled manifest. It MUST NOT return, cache, or audit a successful artifact if any file fails. Byte-for-byte golden ZIP fixtures are required.

The canonical timestamp is always the DOS epoch. Any supplied or imported timestamp before 1980-01-01 or after 2107-12-31 is ignored for canonical ZIP construction rather than clamped; provenance timestamps remain in the manifest only. Implementations MUST reject any internal attempt to emit another ZIP header timestamp as an invariant failure. Cache identity uses the concept/file hash, not timestamps.

## 5. Storage and metadata mapping

### 5.1 Existing generic tables

Every supported concept continues to populate `pgokf.concepts`:

| OKF/parser value | `pgokf.concepts` |
| --- | --- |
| bundle identity | `bundle_id` |
| path-derived `ParsedConcept.id` | `id` |
| Markdown path | `path` |
| `type` | `type` |
| `title` | `title` |
| `description` | `description` |
| `tags` | `tags text[]` |
| `resource` | `resource text` using existing JSON serialization behavior |
| Markdown-derived plain text | `body_text` |
| BLAKE3 source digest | `file_hash` |
| search input | `body_tsv` |
| active tenant | `tenant_id` |

Unknown frontmatter remains one row per key in `pgokf.concept_metadata(bundle_id, concept_id, key, value, tenant_id)`. At minimum:

- Code Snippet: `language`, `visibility`, `author`, `source`, `license`, `filename`;
- Scaffold Template: `visibility`, `author`, `source`, `template_engine`, `output_format`, `parameters`, `builtin`, `renderer_min_version`;
- Skill: synthesized OKF fields plus the complete original Agent Skills frontmatter under `agent_skill`;
- Script: `language`, `runtime`, `arguments`, `skill_package`, and scan metadata;
- Reference: `format`, `skill_package`, source path, and media metadata.

This generic mapping is the portability contract. Type projections are transactional derived projections; exact payload columns are mandatory backing stores and rebuilds MUST use retained exact source, never `body_text`.

### 5.2 Why exact type projections are required

`body_text` is a Markdown-to-plain-text search projection. It is not an exact code/template store and cannot reliably reconstruct fences, whitespace, file descriptors, or CTL1 trim markers. `pgokf.concept_source` is exact but optional (`store_source=false` by default). Scaffold rendering and exact snippet retrieval therefore MUST use dedicated type projections created during the sync transaction.

### 5.3 Mandatory type and resource projections

All five types have mandatory projections. `body_text` is never an exact-payload store. The following DDL is normative in shape; implementations MAY add indexes but MUST preserve the named columns and semantics.

```sql
CREATE TABLE pgokf.code_snippets (
  bundle_id bigint NOT NULL, concept_id text NOT NULL, language text NOT NULL,
  visibility text NOT NULL CHECK (visibility IN ('public','internal','private')),
  author jsonb NOT NULL, source jsonb, license text, filename text,
  code_text text NOT NULL, examples_text text NOT NULL DEFAULT '', code_tsv tsvector,
  source_file_hash text NOT NULL, tenant_id text NOT NULL,
  PRIMARY KEY (bundle_id, concept_id),
  FOREIGN KEY (bundle_id, concept_id) REFERENCES pgokf.concepts(bundle_id,id) ON DELETE CASCADE
);
CREATE TABLE pgokf.scaffold_templates (
  bundle_id bigint NOT NULL, concept_id text NOT NULL,
  visibility text NOT NULL CHECK (visibility IN ('public','internal','private')),
  author jsonb NOT NULL, source jsonb, template_engine text NOT NULL CHECK (template_engine='CTL1'),
  template_protocol_version integer NOT NULL CHECK (template_protocol_version=1),
  output_format text NOT NULL CHECK (output_format='zip'), parameters jsonb NOT NULL,
  template_manifest jsonb NOT NULL, template_source bytea NOT NULL,
  renderer_min_version text, source_file_hash text NOT NULL, tenant_id text NOT NULL,
  PRIMARY KEY (bundle_id, concept_id),
  FOREIGN KEY (bundle_id, concept_id) REFERENCES pgokf.concepts(bundle_id,id) ON DELETE CASCADE
);
CREATE TABLE pgokf.skills (
  bundle_id bigint NOT NULL, concept_id text NOT NULL,
  visibility text NOT NULL CHECK (visibility IN ('public','internal','private')),
  agent_skill jsonb NOT NULL, skill_md bytea NOT NULL, package_root text NOT NULL,
  package_hash text NOT NULL, source_file_hash text NOT NULL, tenant_id text NOT NULL,
  PRIMARY KEY (bundle_id, concept_id),
  FOREIGN KEY (bundle_id, concept_id) REFERENCES pgokf.concepts(bundle_id,id) ON DELETE CASCADE
);
CREATE TABLE pgokf.scripts (
  bundle_id bigint NOT NULL, concept_id text NOT NULL, language text NOT NULL,
  visibility text NOT NULL CHECK (visibility IN ('public','internal','private')),
  author jsonb, source jsonb, license text, runtime jsonb, arguments jsonb,
  exit_codes jsonb, exact_bytes bytea NOT NULL, byte_size bigint NOT NULL,
  executable_sha256 text NOT NULL, source_path text NOT NULL, package_concept_id text,
  source_file_hash text NOT NULL, script_tsv tsvector, tenant_id text NOT NULL,
  PRIMARY KEY (bundle_id, concept_id),
  FOREIGN KEY (bundle_id, concept_id) REFERENCES pgokf.concepts(bundle_id,id) ON DELETE CASCADE
);
CREATE TABLE pgokf.references (
  bundle_id bigint NOT NULL, concept_id text NOT NULL,
  visibility text NOT NULL CHECK (visibility IN ('public','internal','private')),
  format text NOT NULL, media_type text NOT NULL, author jsonb, source jsonb,
  license text, exact_bytes bytea NOT NULL, byte_size bigint NOT NULL,
  content_sha256 text NOT NULL, text_body text, extracted_text text,
  extraction jsonb, source_path text NOT NULL, package_concept_id text,
  source_file_hash text NOT NULL, reference_tsv tsvector, tenant_id text NOT NULL,
  PRIMARY KEY (bundle_id, concept_id),
  FOREIGN KEY (bundle_id, concept_id) REFERENCES pgokf.concepts(bundle_id,id) ON DELETE CASCADE
);
```

`source_file_hash` equals the parent `concepts.file_hash`. For virtual resources, that parent hash is the BLAKE3 digest of exact resource bytes plus canonical virtual metadata; `executable_sha256`/`content_sha256` are SHA-256 of exact returned bytes. `scripts.exact_bytes` is mandatory even when UTF-8; converting through `body_text`, newline normalization, or a database text encoding is forbidden. `skills.skill_md` preserves exact `SKILL.md` source and `agent_skill` preserves the complete parsed frontmatter; no additional typed payload table is required. `references.exact_bytes` is mandatory for text and binary References; extraction is derived, bounded, labeled with extractor/version, and never replaces exact bytes.

Every projection carries `tenant_id`, enables and forces the authorization model in §7.4, cascades on concept deletion, is transactionally maintained, and is not directly writable by application roles. Rebuild uses retained `concept_source`, `skills.skill_md`, or the mandatory exact type/resource stores; it MUST report an unrebuildable source rather than use `body_text`.

### 5.4 Raw staging, classification, and projection pipeline

Sync MUST stage the complete source snapshot before calling `parse_concept`. Each entry contains normalized confined path, exact bytes, byte length, lowercase BLAKE3 hash, adapter/source kind, snapshot ID, owning package root if any, and one source-file class. Classification is deterministic and uses this precedence:

1. basename `index.md` or `log.md` → `Ignored` by this classifier and handled by existing reserved-file logic;
2. exact case-sensitive basename `SKILL.md` → `SkillManifest`;
3. within the nearest containing Skill package, `scripts/**` → `SkillScript`, `references/**` → `SkillReference`, `assets/**` → `SkillAsset`;
4. `.md` with frontmatter containing non-empty `type` and `title` → `OkfDocument`;
5. everything else → `Ignored`.

The nearest ancestor with `SKILL.md` owns a resource; nested packages form a new ownership boundary. A path cannot have multiple owners. Ambiguous case collisions, generated-ID collisions, nested ownership ambiguity, invalid path normalization, and a `.md` that appears intended as OKF but lacks required fields produce stable diagnostics and follow strict/warn policy.

Optional package metadata has exactly one reserved filename, `.okf-package.yaml`, at the package root. It is `Ignored` as a concept but included in package hashing. Its schema is:

```yaml
version: 1
resources:
  scripts/check.sh:
    type: Script
    title: Check replication lag
    description: Read-only replication health check.
    language: bash
    runtime: {executable: /usr/bin/env bash}
    visibility: internal
  assets/topology.png:
    type: Reference
    title: Failover topology
    description: Cluster topology diagram.
    format: image/png
    media_type: image/png
    visibility: internal
```

Keys under `resources` are NFC-normalized package-relative paths resolved once and confined to that package. They MUST name an existing `scripts/`, `references/`, or `assets/` file; duplicate normalized keys, unknown top-level/resource keys, type/path-class mismatch, or a metadata entry without a file are diagnostics. Script metadata requires title, description, language, and visibility unless deterministically supplied by the owning Skill visibility plus unambiguous filename/shebang rules; Reference metadata requires title, format/media type, and visibility. Explicit metadata wins over safe heading/filename/shebang inference, but conflicts with verified magic bytes, class, or owning visibility are errors. The package hash covers exact sidecar bytes and resulting canonical metadata. For record-at-a-time standalone binary References, the same resource object is supplied as an explicit API argument and is hashed with exact bytes; no undeclared sidecar naming convention is accepted.

Only `OkfDocument` is passed unchanged to `parse_concept`. `SkillManifest` is parsed as Agent Skills source and transformed in memory to a virtual `ParsedConcept` equivalent with `type="Skill"`, `title=<name>`, description from frontmatter, path-derived ID based on `SKILL.md`, body after frontmatter, and complete original frontmatter under `metadata.agent_skill`; the ordinary parser MUST NOT be called on untransformed `SKILL.md`. `SkillScript`, `SkillReference`, and `SkillAsset` MUST NOT go through `parse_concept`: the package projector creates virtual generic rows plus mandatory typed rows directly from staged exact bytes. Binary files under `scripts/` are rejected as Scripts (or, by explicit package metadata, classified as assets/References); they are never decoded lossily.

The transactional order is: stage and enforce limits; determine package ownership; classify; parse ordinary OKF Markdown/transform Skill manifests/materialize raw resources; validate; upsert generic rows; upsert all typed projections; reconcile links and diagnostics; resolve graph bundle-wide; reconcile history/scans/search vectors; then finalize bundle hash/audit/notification. Strict errors roll back all work. Warn mode preserves a safe generic row where possible, omits unsafe typed projection, and persists diagnostics. Ignored entries remain part of package and bundle aggregate hashes when policy says they affect membership, but do not create concepts.

### 5.5 Diagnostics

```sql
CREATE TABLE pgokf.concept_diagnostics (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  bundle_id bigint NOT NULL REFERENCES pgokf.bundles(id) ON DELETE CASCADE,
  concept_id text, concept_identity jsonb NOT NULL, tenant_id text NOT NULL,
  snapshot_hash text NOT NULL, file_hash text, package_hash text,
  validator_version text NOT NULL, policy_version text NOT NULL,
  code text NOT NULL, severity text NOT NULL
    CHECK (severity IN ('info','warning','error','fatal')),
  phase text NOT NULL, path text NOT NULL,
  byte_start bigint, byte_end bigint, line integer, column_no integer,
  message text NOT NULL, remediation text, details jsonb NOT NULL DEFAULT '{}',
  created_at timestamptz NOT NULL DEFAULT now(),
  diagnostic_key text GENERATED ALWAYS AS
    (encode(digest(concat_ws(E'\x1f', code, path, concept_identity::text,
      coalesce(byte_start,-1)::text, coalesce(byte_end,-1)::text), 'sha256'),'hex')) STORED,
  UNIQUE (bundle_id, snapshot_hash, validator_version, policy_version, diagnostic_key)
);
```

Ranges are half-open UTF-8 byte offsets. Diagnostics are a projection of one snapshot and policy: a successful reconciliation atomically replaces diagnostics for touched concepts/packages and deletes stale rows for fixed/deleted artifacts. `fatal` always aborts; `error` aborts strict mode and is retained in warn mode only when a safe generic projection exists; `warning` and `info` never mutate source. Scanner finding details are stored in the restricted scan tables below and generic diagnostics contain only redacted summaries.

`pgokf.get_concept_diagnostics(bundle_id, concept_id, include_info boolean DEFAULT false)` is a `STABLE SECURITY DEFINER` visibility-aware function. It returns code, severity, phase, path/range, message, remediation, validator/policy versions, and redacted details. Unknown and unauthorized identities return the same `22023` result. Search returns only authorized counts by severity.

### 5.6 Relationship migration and edge identity

Migrate, do not replace, `pgokf.links`:

```sql
ALTER TABLE pgokf.links
  ADD COLUMN target_bundle_id bigint,
  ADD COLUMN raw_target text,
  ADD COLUMN resolution_status text NOT NULL DEFAULT 'unresolved'
    CHECK (resolution_status IN ('resolved','unresolved','external','hidden')),
  ADD COLUMN derivation jsonb NOT NULL DEFAULT '[]',
  ADD COLUMN edge_metadata jsonb NOT NULL DEFAULT '{}',
  ADD COLUMN normalized_target_key text;
ALTER TABLE pgokf.links ADD CONSTRAINT links_target_bundle_fk
  FOREIGN KEY (target_bundle_id) REFERENCES pgokf.bundles(id) ON DELETE CASCADE;
CREATE UNIQUE INDEX links_logical_edge_idx ON pgokf.links
  (bundle_id, source_id, link_relation, coalesce(target_bundle_id,-1),
   coalesce(target_id, normalized_target_key));
```

`target_bundle_id` plus `target_id` is resolved concept identity; same-bundle legacy rows backfill `target_bundle_id=bundle_id` when `resolved`. Exact `raw_target` cannot be reconstructed from legacy rows and is backfilled by reparsing retained source when available, otherwise NULL with `derivation:[{"kind":"legacy"}]`. `link_relation` stores canonical uppercase `USES`, `REFERENCES`, `INSTANTIATES`, `DEMONSTRATES`, `REQUIRES`, `RELATED_TO`, or generic `REFERENCE`; legacy values are normalized during migration. The existing positional primary key remains a compatibility key. `normalized_target_key` is populated by the projector with the exact once-decoded, fragment-free, normalized unresolved/external target; it is NULL only for resolved concept targets. The unique index above is the logical edge key. During migration, `link_relation` default changes from lowercase `reference` to uppercase `REFERENCE`; all existing values are uppercased, unknown legacy values remain uppercase generic relationships, and all new writes use canonical values. `resolved` is maintained as `(resolution_status='resolved')`, `is_external` as `(resolution_status='external')`, `target_path` as the normalized internal path when present, and `link_kind` continues to record Markdown syntax kind (`reference` for explicit/package edges). A deferred consistency trigger rejects disagreement among legacy and new columns. Backfill runs under the bundle lock, reparses retained sources, then bundle-wide re-resolves before the unique index is validated.

Explicit typed edges use frontmatter:

```yaml
relationships:
  - relation: USES
    target: ./scripts/check.sh
    metadata: {optional: false}
  - relation: REQUIRES
    target: tool://psql
    metadata: {minimum_version: "15"}
```

Each item has exactly `relation`, `target`, and optional JSON object `metadata`; unknown keys or a noncanonical relation are diagnostics. Relative paths resolve from the source document directory; Skill package membership paths resolve from package root. URI percent-decoding occurs exactly once, query is rejected for internal resources, fragment is retained as edge metadata but excluded from path identity, then separators/path segments are normalized and confinement checked. Resolution precedence is explicit `(bundle_id,concept_id)` metadata when authorized, then normalized bundle-relative concept/resource path, then package-relative virtual resource path, then external URI.

Virtual resource paths resolve through their mandatory `source_path` to generated identities. `REQUIRES` targets `tool:<name>` or `tool://<name>` are external URI nodes, never a `Tool` concept type: `is_external=true`, `resolution_status='external'`, `target_bundle_id/target_id=NULL`, normalized tool URI in `raw_target`.

One logical edge is deduplicated by source identity + canonical relation + resolved target identity, or normalized unresolved/external target. Explicit, Markdown, and membership occurrences coalesce; `derivation` is a deterministically sorted array of all `{kind,path,byte_start,byte_end,ordinal}` occurrences. Explicit metadata wins over inferred metadata only for non-conflicting keys. Conflicting relations or metadata produce diagnostics rather than duplicate contradictory edges. Invisible cross-tenant/private targets are represented to callers as `hidden` without leaking target identity.

## 6. Source adapters

All adapters produce complete raw source snapshots containing OKF Markdown and recognized Skill package resources. They share pgokf's incremental BLAKE3 classification and transaction semantics after bytes enter a supported `ByteSource`.

### 6.1 Local filesystem

No new source type is required.

```sql
SELECT * FROM pgokf.register_bundle(
  '/srv/knowledge/code-catalogue',
  'code-catalogue',
  '{"store_source":true,"validate_code_catalogue":"strict"}'::jsonb
);
```

`refresh_bundle(bundle_id)` rescans filesystem bundles. Existing path canonicalization, symlink-escape checks, exclusions, file count/size limits, reserved `index.md`/`log.md` handling, advisory locking, and changed-file-only parsing remain in force.

### 6.2 Generic Git repositories

A companion (`okf-sync`, `pgokf-ingest`, CI job, or operator service) SHALL:

1. clone/fetch into a controlled worktree;
2. resolve the configured ref to an immutable commit;
3. optionally select a bundle subdirectory;
4. enforce repository URL/ref allowlists and checkout limits;
5. call `register_bundle` on the checked-out filesystem tree, or stream `(path, bytes)` to `register_bundle_content`;
6. record repository URL, resolved revision, subdirectory, and adapter version in `pgokf.bundles.options` and concept `source` metadata.

The current `okf-sync` discovery layer and extension `ByteSource` do not implement network Git cloning as a native PostgreSQL source. Git support is adapter orchestration around existing filesystem/content sync. This avoids network credentials and unbounded repository operations inside a PostgreSQL backend.

### 6.3 GitHub repositories

GitHub is a specialized Git/content adapter, not a trust tier. Configuration SHOULD include:

```json
{
  "adapter": "github",
  "repository": "acme/code-knowledge",
  "ref": "main",
  "subdirectory": "okf",
  "include": ["**/*.md", "**/scripts/**", "**/references/**", "**/assets/**"],
  "concept_types": ["Code Snippet", "Scaffold Template", "Script", "Skill", "Reference"],
  "resolved_revision": "<commit-sha>"
}
```

The adapter:

- fetches with a GitHub App installation token or deploy key outside PostgreSQL;
- resolves branch/tag to a commit SHA before ingestion;
- enumerates all safe bundle-relative paths required for recognized packages after include/exclude rules; it MUST NOT Markdown-filter before package classification;
- parses enough frontmatter to select configured standalone types including `Code Snippet`, `Scaffold Template`, and `Script`, and also supply complete detected Skill packages, while still supplying reserved root `index.md` and relevant `log.md` files;
- supplies exact bytes through `register_bundle_content`, or checks out a filesystem bundle;
- follows pagination and rate-limit backoff;
- does not log tokens or embed them in bundle options/source metadata;
- treats a force-push as a new resolved revision and ordinary content diff;
- does not follow symlinks outside a checked-out root;
- records immutable `source.url` values when possible.

A native future `source_type='github'` requires a migration to the bundles check constraint, credential references, refresh semantics, and a new `ByteSource`; it is explicitly outside extension version 0.2.1.

### 6.4 Database and application content

The existing mountless API is the bulk path:

```sql
SELECT * FROM pgokf.register_bundle_content(
  'application-code-catalogue',
  ARRAY['snippets/retry.md', 'scaffolds/rust-cli.md'],
  ARRAY[convert_to($retry$...$retry$, 'UTF8'),
        convert_to($scaffold$...$scaffold$, 'UTF8')],
  '{"store_source":true,"validate_code_catalogue":"strict"}'::jsonb
);
```

`paths` and `contents` must have equal lengths; paths are bundle-relative and traversal-free. Re-calling it for the same content bundle is a full snapshot resync: omitted paths are deletions. An application MUST NOT use a one-file `register_bundle_content` call against a multi-file bundle.

For record-at-a-time authoring, implement the transactional API in §8.5. It maintains a content bundle's durable per-path source set, reconstructs the full content snapshot under the same advisory lock, and then invokes the ordinary sync engine. Direct `INSERT` into `pgokf.concepts`, metadata, or type projection tables is forbidden because it bypasses hashing, links, FTS, provenance, history, RLS invariants, and type validation.

### 6.5 Distribution between pgokf instances

Code concepts remain ordinary bundle files. Existing source export/content ingest can distribute them when source was retained. Type projection tables are rebuilt at the destination; they are not authoritative transfer records. Transfers SHOULD verify source BLAKE3 hashes and preserve bundle-relative paths, OKF version, provenance, and immutable source revisions.

## 7. Search and authorization

### 7.1 Backward-compatible generic search

Existing `pgokf.concept_search(query, bundle_id, limit_count, concept_type, tags, status, trust_tier, after_cursor)` and the positional `concept_search_result(bundle_id, concept_id, path, title, type, rank, headline)` ABI remain unchanged. Its authorized input relation MUST exclude Script rows whose current status is `findings`, `blocked`, `malicious`, `scan_error`, `unsupported`, or `stale`; because the legacy composite cannot label safety, unsafe/indeterminate Scripts are never returned there. Clients needing labeled all-status results use `catalogue_search`. It searches active bundles and keeps current `websearch_to_tsquery`, ALL-of tags, rank ordering, cursor validation, and 1..500 limit. No field is added to this composite; consumers needing descriptions/tags join through an authorized API, not by assuming extra fields.

### 7.2 Enriched unified search

```sql
pgokf.catalogue_search(
  query text, bundle_id bigint DEFAULT NULL, concept_type text DEFAULT NULL,
  language text DEFAULT NULL, tags text[] DEFAULT NULL,
  visibility text DEFAULT NULL, status text DEFAULT NULL, trust_tier text DEFAULT NULL,
  script_safety text DEFAULT 'safe_or_unknown', limit_count integer DEFAULT 20,
  after_cursor jsonb DEFAULT NULL
) RETURNS SETOF pgokf.catalogue_search_result
```

The enriched result contains, in fixed order: `bundle_id, concept_id, path, title, description, type, language, tags, visibility, rank, headline, trust jsonb, resources jsonb, diagnostics jsonb, script_safety text`. It joins generic concepts to typed projections, provenance, diagnostics, and scan summaries after authorization; it never embeds exact Script/Reference/template bytes or restricted scanner details.

The query parser is `websearch_to_tsquery` (or configured safe equivalent), with bounded UTF-8 bytes/tokens; raw caller-controlled `to_tsquery` syntax is forbidden. Ranking is fixed for ranking version `catalogue-rank-1`:

`rank = 0.55*ts_rank_cd(common_tsv,q) + 0.25*ts_rank_cd(type_tsv,q) + 0.10*exact_title + 0.05*exact_language + 0.05*verified_trust`, clamped to `[0,1]` after each component is normalized to `[0,1]`. `common_tsv` weights title A, tags/type/description B, body D. Type vectors weight Code Snippet/Script exact tokens C, Scaffold parameter names/output paths C, Skill procedure headings/body C, Reference extracted/text body C. Unsafe/blocked Scripts are excluded by default; authorized `script_safety='all'` includes them labeled and applies a 0.25 multiplicative demotion. Unknown/unscanned Scripts remain labeled `unknown` and are never described as clean.

Stable order is `rank DESC,bundle_id ASC,concept_id ASC`. Cursor is `{"ranking_version":"catalogue-rank-1","parser_version":<version>,"rank":number,"bundle_id":integer,"concept_id":string}`; mismatched versions or malformed values raise `22023`. Facets are computed after tenant, visibility, active bundle, type, language, lifecycle/trust, and script-safety filters and before pagination.

### 7.3 Type-specific search

`search_code` delegates to the authorized unified relation, adds exact language and `code_tsv` behavior, and returns the existing documented code result. `search_scripts(query,language,scan_status,...)` returns summaries only and defaults to excluding `blocked`, `malicious`, and `scan_error`. `search_references(query,format,media_type,...)` searches authorized descriptive/extracted text without binary bytes. `search_scaffolds` and `search_skills` are convenience wrappers. Every wrapper uses the same common filters, rank formula, versioned cursor, active-bundle predicate, and visibility enforcement; no wrapper concatenates separately ranked pages.

### 7.4 Complete visibility enforcement

Visibility is mandatory for all five types. File-authored concepts MUST declare it; API-created and virtual package resources inherit the owning Skill visibility unless explicit package metadata narrows it. A child MUST NOT be more visible than its owner. Authorization classes are:

| Caller | Rows visible |
| --- | --- |
| anonymous/public endpoint role | `public` only |
| authenticated `pgokf_reader` for effective tenant | `public`, `internal` |
| reader with transaction-local private capability | `public`, `internal`, `private` |
| writer/admin | no broader read visibility unless separately granted |

The private capability is a signed/validated gateway decision installed as transaction-local `pgokf.private_read='on'` only by a non-login capability-setter role; applications cannot set it directly. Effective tenant is mandatory for application roles; unset tenant MUST deny rather than retain legacy see-all behavior. Owner/superuser bypass roles MUST NOT be used by applications.

Base catalogue/type/link/diagnostic/scan tables have all privileges revoked from application roles. RLS is enabled and forced with tenant + visibility policies using `pgokf_private.can_read(tenant_id,visibility)`. Reader access is only through `security_barrier` views or pinned-search-path `SECURITY DEFINER` functions that call the same predicate. Search, facets, retrieval, source/history, links, diagnostics, rendering, export, and resource download all enforce it. A caller visibility filter only narrows authorized rows. Missing, cross-tenant, and invisible identities have indistinguishable errors; hidden targets do not affect exposed counts/facets/diagnostics/resource summaries. Writes stamp the effective tenant and reject visibility widening of package children.

## 8. SQL API

### 8.1 Roles and security

Follow existing roles:

- `pgokf_reader`: search, metadata retrieval, exact snippet retrieval, scaffold manifest retrieval, and rendering subject to visibility policy;
- `pgokf_writer`: content upsert/delete and all reader operations;
- `pgokf_admin`: projection rebuilds, renderer configuration, limits, and optional server-side export.

Every function accessing tenant data MUST apply the existing effective tenant logic and RLS. Security-definer functions MUST pin `search_path`, schema-qualify objects, validate inputs before dynamic operations, and revoke execution from `PUBLIC`.

### 8.2 Snippet retrieval

```sql
pgokf.get_code_snippet(
  bundle_id bigint,
  concept_id text
) RETURNS pgokf.code_snippet_result
```

```sql
CREATE TYPE pgokf.code_snippet_result AS (
  bundle_id bigint,
  concept_id text,
  path text,
  title text,
  description text,
  language text,
  tags text[],
  visibility text,
  author jsonb,
  source jsonb,
  license text,
  filename text,
  code text,
  examples text,
  file_hash text,
  modified_at timestamptz
);
```

Unknown identity, wrong concept type, missing typed projection, or invisible row raises SQLSTATE `22023` without leaking cross-tenant existence. Exact code comes from `code_snippets.code_text`, not `body_text`.

### 8.3 Scaffold retrieval and validation

```sql
pgokf.get_scaffold(
  bundle_id bigint,
  concept_id text
) RETURNS pgokf.scaffold_result

pgokf.validate_scaffold_parameters(
  bundle_id bigint,
  concept_id text,
  parameters jsonb,
  target_platform text DEFAULT 'portable',
  line_endings text DEFAULT 'lf',
  output text DEFAULT 'zip'
) RETURNS jsonb
```

`get_scaffold` returns metadata, parameter schema, validated file manifest, file hash, and renderer compatibility, but not raw private source unless authorized. `validate_scaffold_parameters` returns the normalized supplied/defaulted public variable object plus a public input digest; it excludes computed variables from caller input and rejects unknown properties.

### 8.4 Rendering

```sql
pgokf.render_scaffold_zip(
  bundle_id bigint,
  concept_id text,
  parameters jsonb,
  target_platform text DEFAULT 'portable',
  line_endings text DEFAULT 'lf'
) RETURNS bytea
```

Optional metadata-rich form:

```sql
pgokf.render_scaffold(
  bundle_id bigint,
  concept_id text,
  parameters jsonb,
  output text DEFAULT 'zip',       -- zip | manifest | json
  target_platform text DEFAULT 'portable',
  line_endings text DEFAULT 'lf'
) RETURNS pgokf.scaffold_render_result
```

```sql
CREATE TYPE pgokf.scaffold_render_result AS (
  media_type text,
  artifact bytea,
  manifest jsonb,
  concept_file_hash text,
  renderer_version text,
  input_sha256 text,
  output_sha256 text,
  file_count integer,
  byte_size bigint
);
```

For `manifest`, `artifact` is NULL and file bodies are omitted. For `json`, artifact is UTF-8 JSON bytes including bodies and is subject to a stricter response limit. For `zip`, artifact is canonical ZIP. The manifest includes template protocol version, renderer software version, target platform, line-ending mode, normalized non-secret variables, every output path/hash/mode/size, source concept identity, bundle, file hash, origin/source metadata, author, and local trust state.

Errors:

| SQLSTATE | Condition |
| --- | --- |
| `22023` | malformed identity, unsupported target/output, parameter/schema/type/path validation, unknown or inaccessible scaffold; details use structured JSON where practical |
| `54000` | output count/size/path/nesting/time resource limit |
| `0A000` | renderer/template version not supported by this build |
| `XX000` | internal renderer/archive invariant failure; no partial output |

The HTTP/MCP layer MAY map validation to 422, limits to 413, path collisions/no renderable version to 409, and internal errors to 500. SQL functions themselves use PostgreSQL SQLSTATEs.

The render digest MUST cover at least tenant-insensitive immutable concept file hash, template protocol version, renderer software version, normalized parameters, target platform, line endings, and output representation. Cache keys MUST NOT depend on title or mutable database row IDs alone.

A server-side directory writer is not part of the reader API. If implemented, `pgokf_admin` only:

```sql
pgokf.export_scaffold(
  bundle_id bigint,
  concept_id text,
  parameters jsonb,
  dest_dir text,
  target_platform text DEFAULT 'portable',
  line_endings text DEFAULT 'lf'
) RETURNS pgokf.export_result
```

It MUST reuse existing `export_sources` path-confinement/no-follow patterns, refuse an unsafe/non-empty destination unless an explicit policy allows it, materialize through a temporary sibling, fsync as configured, and atomically rename. The preferred interface remains ZIP return to an external service.

### 8.5 Record-at-a-time content authoring

Provide a generic safe primitive:

```sql
pgokf.put_concept_content(
  bundle_name text,
  path text,
  content bytea,
  options jsonb DEFAULT '{}'
) RETURNS pgokf.bundle_sync_result

pgokf.delete_concept_content(
  bundle_name text,
  path text
) RETURNS pgokf.bundle_sync_result
```

These operate only on `source_type='content'` bundles managed by this primitive. They acquire the bundle advisory lock, update a durable private source-file set, and run the same full snapshot/diff/project pipeline. They MUST NOT be implemented as direct generic/type-table DML.

Convenience wrappers generate canonical Markdown and then call that primitive:

```sql
pgokf.put_code_snippet(
  bundle_name text,
  path text,
  title text,
  language text,
  code text,
  description text DEFAULT NULL,
  tags text[] DEFAULT '{}',
  visibility text DEFAULT 'internal',
  author jsonb DEFAULT NULL,
  source jsonb DEFAULT NULL,
  examples text DEFAULT NULL,
  extra_frontmatter jsonb DEFAULT '{}'
) RETURNS pgokf.bundle_sync_result

pgokf.put_scaffold_template(
  bundle_name text,
  path text,
  title text,
  parameters jsonb,
  computation text,
  description text DEFAULT NULL,
  tags text[] DEFAULT '{}',
  visibility text DEFAULT 'internal',
  author jsonb DEFAULT NULL,
  source jsonb DEFAULT NULL,
  output_format text DEFAULT 'zip',
  extra_frontmatter jsonb DEFAULT '{}'
) RETURNS pgokf.bundle_sync_result
```

Wrappers validate reserved frontmatter keys so `extra_frontmatter` cannot override type, title, language, parameters, template engine, output format, visibility, author, source, or tags. They choose a fence length that safely encloses arbitrary payloads and produce deterministic YAML/Markdown bytes. `put_scaffold_template.computation` is the complete `# Computation` fenced-block envelope, not raw unlabelled CTL1.

### 8.6 Skill, Script, Reference, diagnostics, and scan retrieval

```sql
pgokf.get_skill(bundle_id bigint, concept_id text) RETURNS pgokf.skill_result;
pgokf.get_script(bundle_id bigint, concept_id text) RETURNS pgokf.script_result;
pgokf.get_reference(bundle_id bigint, concept_id text, include_bytes boolean DEFAULT true)
  RETURNS pgokf.reference_result;
pgokf.get_script_scan_status(bundle_id bigint, concept_id text)
  RETURNS pgokf.script_scan_status_result;
```

`get_skill` returns metadata, full Skill Markdown body/source as authorized, package hash, relationship/resource summaries, and diagnostic counts. `get_script` returns exact `bytea` from `scripts.exact_bytes`, byte/hash identity, typed runtime/arguments/exit codes, scan summary, and authorization requirements; it MUST NOT transcode or return `body_text`. History retrieval for Script versions MUST return exact historical bytes keyed by immutable file hash. When history is enabled the following payload table is mandatory (an equivalent FK to the installation's SCD-2 history identity is allowed):

```sql
CREATE TABLE pgokf.script_history_payloads (
  bundle_id bigint NOT NULL, concept_id text NOT NULL, file_hash text NOT NULL,
  valid_from timestamptz NOT NULL, exact_bytes bytea NOT NULL, byte_size bigint NOT NULL,
  executable_sha256 text NOT NULL, typed_metadata jsonb NOT NULL, tenant_id text NOT NULL,
  PRIMARY KEY(bundle_id,concept_id,file_hash,valid_from)
);
CREATE TABLE pgokf.reference_history_payloads (
  bundle_id bigint NOT NULL, concept_id text NOT NULL, file_hash text NOT NULL,
  valid_from timestamptz NOT NULL, exact_bytes bytea NOT NULL, byte_size bigint NOT NULL,
  content_sha256 text NOT NULL, typed_metadata jsonb NOT NULL, tenant_id text NOT NULL,
  PRIMARY KEY(bundle_id,concept_id,file_hash,valid_from)
);
```

Both tables have forced tenant/visibility RLS through a join to the corresponding history identity, cascade or are pruned atomically with that identity, and are application read-only. `get_script_version(bundle_id,concept_id,file_hash,valid_from)` and `get_reference_version(...)` return authorized exact historical bytes with the same non-disclosure behavior. A history row without its exact payload is invalid.

`get_reference` returns metadata, format/media type, exact SHA-256, size, bounded text/extraction, and optionally exact bytes. Large/binary References SHOULD be delivered by a capability-aware streaming service that binds `(bundle_id,concept_id,content_sha256)` and enforces range/size limits; SQL still defines exact-byte semantics. All four functions use §7.4 non-disclosure behavior. `get_concept_diagnostics` is defined in §5.5.

Standalone authoring wrappers `put_script` and `put_reference` MUST produce canonical `type: Script`/`type: Reference` OKF documents or complete raw package snapshots and invoke the ordinary sync pipeline; direct projection DML is forbidden.

### 8.7 Maintenance

```sql
pgokf.rebuild_code_catalogue_projection(
  bundle_id bigint DEFAULT NULL,
  validate_mode text DEFAULT 'strict'
) RETURNS pgokf.projection_rebuild_result

pgokf.code_catalogue_stats()
  RETURNS pgokf.code_catalogue_stats
```

A rebuild requires retained `concept_source` or another exact source snapshot. It MUST report concepts that cannot be rebuilt because source was not stored. It MUST not pretend `body_text` is sufficient.

## 9. Sync, history, and package reconciliation

All projection work occurs in the existing atomic bundle transaction. Concept `file_hash` remains lowercase BLAKE3. In addition, every Skill has `package_hash = BLAKE3` over a domain-separated encoding of classifier version, validator/policy versions, exact `SKILL.md` hash, and sorted records `class + NUL + package-relative-path + NUL + byte-hash + LF` for every owned member. Bundle `sync_hash` continues to cover all staged snapshot records.

An unchanged concept may remain untouched only when its file hash, package hash/dependency hash, classifier version, validator version, policy version, and relevant scan/search versions are all unchanged. Add/update/delete/move/class change of any package member invalidates the owning Skill even when `SKILL.md` bytes are identical. Reconciliation MUST recompute the Skill projection, virtual Script/Reference rows, membership and Markdown edges, package diagnostics, portability/client/trust/resource summaries, scan eligibility, and unified vectors. Moving a resource across package boundaries invalidates both owners. Creating/removing a nested `SKILL.md`, deleting a package, or a resource ceasing to qualify performs the same closure and deletes stale virtual rows/edges/diagnostics by snapshot ownership.

Changed and dependency-invalidated IDs are included in audit/change manifests and notifications without payload bytes. Link resolution is recomputed bundle-wide so unchanged sources respond to added/removed targets. Strict failure rolls back generic/type/resource rows, history, scans, diagnostics, links, hashes, logs, and notifications. Warn mode never preserves an old typed projection for changed bytes. A typed row's source hash MUST equal its generic parent hash at commit.

Script history, when enabled, stores exact bytes, byte size, SHA-256, source file hash, and typed metadata for every retained SCD-2 version; `body_text` is not history. Reference history follows the same exact-byte rule. Retention removes metadata and payload atomically.

## 10. UI interface contract

The UI is a separate application. It is not shipped as part of the PostgreSQL extension and MUST NOT own catalogue semantics.

### 10.1 UI responsibilities

The UI MAY provide:

- search box, language selector, tags/facets, lifecycle/trust filters;
- snippet detail with exact code, metadata, provenance, copy/download controls;
- scaffold detail generated from `parameters` schema;
- client-side form validation for responsiveness, followed by mandatory server validation;
- render request and ZIP download;
- source/revision links and provenance display;
- warnings that CTL1 performs deterministic substitution without contextual escaping;
- access-denied behavior that does not reveal cross-tenant/private existence.

The UI MUST NOT:

- query base tables with an unrestricted owner role;
- reconstruct code from search headlines or `body_text`;
- implement a divergent CTL1 renderer for authoritative downloads;
- infer trust from `builtin`, GitHub stars, or repository ownership alone;
- write directly to catalogue tables;
- execute snippets or generated output automatically.

### 10.2 Query flow

1. Search snippets with `search_code` or browse with `discover_code`.
2. Populate filters/facets through a server endpoint backed by indexed SQL functions.
3. Fetch a selected snippet with `get_code_snippet`.
4. Search scaffolds with `search_scaffolds`.
5. Fetch metadata/schema with `get_scaffold`.
6. Build a form from the returned typed parameter schema.
7. Submit exact JSON values, `target_platform`, and `line_endings` to `render_scaffold`.
8. Download returned ZIP with media type, digest, and a safe filename supplied by the API layer.

The service between UI and PostgreSQL SHOULD translate PostgreSQL composite rows into stable JSON, enforce authentication/session tenant setup, cap request bodies, set statement timeouts, stream ZIP bytes without modifying them, and attach `Digest`, ETag, and safe `Content-Disposition` headers.

### 10.3 Stable response fields

UI-facing JSON should expose:

```json
{
  "identity": {"bundle_id": 12, "concept_id": "snippets/retry"},
  "type": "Code Snippet",
  "title": "Retry an async operation",
  "description": "...",
  "language": "python",
  "tags": ["retry", "async"],
  "visibility": "internal",
  "author": "human:alice",
  "source": {"kind": "github", "url": "...", "revision": "..."},
  "code": "...",
  "file_hash": "...",
  "modified_at": "..."
}
```

Search result schemas are summaries and may evolve additively. Exact retrieval and render schemas are versioned API contracts. The UI SHOULD use `(bundle_id, concept_id)`, not title or declared frontmatter ID, as the database identity.

## 11. Security, scanners, and resource controls

PostgreSQL never executes catalogue content. Code Snippet is text intended to be read/adapted and is not subject to Script execution scanning. Script is exact executable-intent content: retrieval preserves bytes, search labels safety, and execution requires scan plus trust authorization. Renaming or embedding executable content as a Code Snippet MUST NOT confer execution authority.

### 11.1 Script scan persistence and policy

```sql
CREATE TABLE pgokf.script_scans (
  bundle_id bigint NOT NULL, concept_id text NOT NULL, script_sha256 text NOT NULL,
  scanner_set_version text NOT NULL, policy_version text NOT NULL,
  status text NOT NULL CHECK (status IN
    ('pending','clean','findings','blocked','malicious','scan_error','unsupported','stale')),
  highest_severity text CHECK (highest_severity IN ('info','low','medium','high','critical')),
  scanned_at timestamptz, expires_at timestamptz, summary jsonb NOT NULL DEFAULT '{}',
  signature_status text NOT NULL CHECK
    (signature_status IN ('verified','invalid','missing','untrusted','not_required')),
  tenant_id text NOT NULL,
  PRIMARY KEY(bundle_id,concept_id,script_sha256,scanner_set_version,policy_version),
  FOREIGN KEY(bundle_id,concept_id) REFERENCES pgokf.scripts(bundle_id,concept_id) ON DELETE CASCADE
);
CREATE TABLE pgokf.script_scan_findings (
  scan_bundle_id bigint NOT NULL, scan_concept_id text NOT NULL, script_sha256 text NOT NULL,
  scanner_set_version text NOT NULL, policy_version text NOT NULL,
  scanner text NOT NULL, scanner_version text NOT NULL, ruleset_version text NOT NULL,
  finding_code text NOT NULL, severity text NOT NULL
    CHECK (severity IN ('info','low','medium','high','critical')),
  byte_start bigint, byte_end bigint, redacted_message text NOT NULL,
  restricted_details jsonb NOT NULL, tenant_id text NOT NULL,
  PRIMARY KEY(scan_bundle_id,scan_concept_id,script_sha256,scanner_set_version,
              policy_version,scanner,finding_code),
  FOREIGN KEY(scan_bundle_id,scan_concept_id,script_sha256,scanner_set_version,policy_version)
    REFERENCES pgokf.script_scans(bundle_id,concept_id,script_sha256,scanner_set_version,policy_version)
    ON DELETE CASCADE
);
```

Mandatory scanner classes are: secret/credential detection; malware/signature detection for all exact bytes; language-aware static dangerous-operation analysis where a supported scanner exists; and package/source signature verification when a signature is declared or policy requires one. Deployments configure concrete engines, versions, rulesets, timeouts, and accepted signatures in a versioned scanner set. No engine is implicitly trusted: errors/timeouts/unsupported languages yield `scan_error`/`unsupported`, never `clean`. Signature state is the constrained `signature_status` column (`verified`, `invalid`, `missing`, `untrusted`, or `not_required`) and is bound to exact script SHA-256/source revision. If policy requires a signature, `invalid`, `missing`, or `untrusted` forces `status='blocked'`; such a scan can never be `clean`. `verified` means cryptographic verification by an allowed signer, while `not_required` is permitted only when policy does not require signing.

A scan is valid only for exact bytes + scanner set + rulesets + policy and before expiry. Byte/policy/ruleset changes immediately expose `pending` or `stale`; old clean results never carry forward. `high` or `critical` mandatory findings produce `blocked` by default; a malware positive produces `malicious`. Finding details are restricted to a dedicated security role; ordinary readers receive status, highest severity, timestamps, scanner-set version, and redacted counts. Execution authorization requires current `clean` or an explicit audited policy exception, identity/hash recheck, declared argument validation without shell interpolation, explicit capability grants, sandbox/least privilege, and resource/time limits.

### 11.2 Limits and archive safety

Limits apply before allocation and again after decoding, normalization, extraction, rendering, and line-ending conversion. Required ceilings include source file count/bytes, aggregate snapshot bytes, package members/bytes, frontmatter, fence count/info-string bytes, Script/reference bytes, extraction/OCR output, scanner input/time/memory, query bytes/tokens/headline, graph expansion, digest/manifest bytes, CTL depth/time, rendered files/per-file/total bytes, ZIP central-directory bytes, and SQL/HTTP response bytes.

Initial maxima: source file 5 MiB (subject to lower existing limit), snapshot 1 GiB/100,000 files, package 10,000 members/256 MiB, Script 10 MiB, directly returned Reference 25 MiB, extracted text 5 MiB, 10,000 scaffold fences, 1,000 output files, 5 MiB per output file, 25 MiB total output, 1,024 path bytes/240 segment bytes, and 5 MiB JSON output. Operators SHOULD lower these.

Externally supplied ZIP/tar/archive References are never expanded by default. If extraction is enabled, reject traversal/absolute/backslash/NUL paths, symlinks/hardlinks/devices, duplicate or case/NFC-colliding names, nested archives beyond configured depth (default zero), encrypted members, more than 10,000 members, more than 256 MiB uncompressed bytes, more than 64 MiB compressed bytes, or compression ratio above 100:1. Count and size checks use declared and observed values and abort on mismatch. Generated canonical ZIP is STORE-only and still obeys output/central-directory/response ceilings.

Network adapters run outside PostgreSQL with URL/ref allowlists and credential redaction. Security-definer functions pin `search_path`, schema-qualify objects, validate before dynamic work, revoke `PUBLIC`, honor cancellation/statement timeout, and audit identity/hash/digests/counts without payloads or sensitive findings. Licensing/source metadata are preserved. CTL1 has no secrets or contextual escaping and generated output is never auto-executed.

## 12. Implementation plan and acceptance criteria

### Phase 1: parser/projection

- Add pure typed body/frontmatter parsers and fixture suites.
- Add migrations for all five mandatory type projections, diagnostics, links, and scan tables, indexes, grants, and RLS.
- Wire projection into `run_bundle_sync` after generic upsert.
- Add strict/warn/off bundle option.
- Verify generic metadata remains complete.

Acceptance:

- valid sample documents populate generic and typed rows atomically;
- invalid strict documents roll back the sync;
- warn mode preserves generic rows and reports the omitted typed projection;
- changing/deleting type removes stale projections;
- exact code and trim-sensitive CTL1 round-trip from typed projection with `store_source=false`.

### Phase 2: search and retrieval

- Add code-specific `simple`-configuration vector/index.
- Add language/tag/visibility filters and stable cursors.
- Add exact snippet/scaffold/Skill/Script/Reference retrieval, unified search, scan status, diagnostics, and facets.
- Add SQL API documentation and privilege tests.

Acceptance:

- language index is used in representative plans;
- tag filters remain ALL-of;
- cursor traversal has no duplicates or gaps under a stable snapshot;
- disabled/retired/cross-tenant/private concepts are excluded;
- result limits and blank-query behavior match documented SQLSTATEs.

### Phase 3: CTL1 renderer

- Import/version the normative CTL1 grammar and golden fixtures.
- Implement typed parameter normalization and validation.
- Implement path conditions/templates and collision checks.
- Implement text rendering and line endings.
- Implement canonical ZIP and manifest/digests.
- Add cancellation and all resource limits.

Acceptance:

- every upstream CTL1 positive/negative fixture passes;
- golden ZIPs are byte-for-byte identical;
- repeated identical renders produce identical digest and bytes;
- unknown values, type mismatches, cycles, path traversal/collisions, and limit overflows fail without partial output;
- rendered CTL delimiters are not evaluated a second time;
- Windows/macOS/portable collision fixtures are rejected.

### Phase 4: adapters and direct authoring

- Add Git/GitHub companion adapters that feed filesystem/content sync.
- Add durable record-at-a-time content source storage and wrapper functions.
- Record resolved revisions without credentials.
- Add end-to-end ingest/update/delete tests.

Acceptance:

- GitHub branch resolution records a commit SHA;
- a no-change sync preserves `indexed_at`;
- removed remote documents delete projections;
- one-record upsert cannot delete sibling records;
- bulk `register_bundle_content` retains full-snapshot semantics;
- adapter failure leaves the previous successful bundle state intact.

### Phase 5: interface contract

- Add HTTP/MCP mappings without moving semantics out of pgokf.
- Add UI schema examples and error mapping.
- Add ZIP digest/ETag/content-disposition behavior.
- Run tenant/visibility authorization tests.

## 13. Required test matrix

At minimum:

- parser: missing/duplicate headings, missing/multiple schema fences, fence-language mismatch, nested/long fences, Unicode, CRLF, empty payload;
- metadata: string/object author/source, forbidden secret keys, unknown extension keys, OKF provenance coexistence;
- synchronization: classifier precedence, raw resource bypass of `parse_concept`, complete filesystem/content snapshots, strict/warn/off, add/update/remove/move/type/ownership change, package invalidation, rollback, source storage on/off;
- search: legacy composite ABI, unified joins/rank formula/versioned cursors, type wrappers, identifier tokenization, language aliases, tags, visibility, lifecycle/trust, unsafe Script exclusion/demotion/labels, facets, disabled/retired bundles, tenants, max limit;
- CTL1: every grammar production, transforms, whitespace trim, escaped delimiters, one pass, typed equality/contains, each locals/empty else, nesting limit;
- parameters: exact JSON types, unknown/computed input, defaults/dependency cycles, NFC/newlines, numeric exclusions, unique arrays;
- paths: traversal, absolute/backslash/NUL, segment and total size, exact/NFC/case/Windows collisions, platform modes;
- archive: fixed DOS epoch in local/central headers, empty canonical ZIP, ordering, modes, CRC, flags, no extras/ZIP64/data descriptor, deterministic bytes, out-of-range source timestamps ignored;
- security: forced tenant+visibility RLS/capability views, role grants, cross-tenant/private non-disclosure, scanner lifecycle/severity/signatures, archive bombs, unsafe Script search, SQL search path, cancellation, denial-of-service limits;
- API: composite fields, SQLSTATEs, no partial output, digest stability, audit redaction.

## 14. Resolved v0.2.1 decisions

This version has no open normative issue from v0.2.0. CTL1 grammar, normalization, UTF-8 byte offsets, digest objects, null/empty behavior, empty render output, protocol negotiation, fixed ZIP epoch, visibility authorization, raw-source classification, durable content mutation boundary, identifier/ranking versioning, and package invalidation are fixed by §§4–11. Git/GitHub remain companion adapters rather than native `ByteSource` variants. Binary scaffold pass-through remains an explicit non-goal; exact binary Reference storage is supported. Implementations MUST NOT substitute a local guess for these rules.

## 15. Concept type: `Skill`

### 15.1 Portable source and virtual OKF projection

A Skill is authored in the converged Agent Skills package format: a directory containing a required `SKILL.md` and optional `scripts/`, `references/`, and `assets/` subdirectories. `SKILL.md` remains the portable source of truth. pgokf MUST NOT rewrite it to add OKF-only fields. During bundle synchronization pgokf materializes a virtual ordinary OKF concept with `type: Skill`, then submits that concept to the same generic storage, search, provenance, history, and link pipeline as file-authored OKF concepts.

A portable `SKILL.md` begins with YAML frontmatter:

```markdown
---
name: postgresql-failover
description: Diagnose and recover a PostgreSQL primary failure safely.
license: Apache-2.0
compatibility: Requires PostgreSQL 15+ and a POSIX shell.
metadata:
  supported_clients: [hermes-agent, claude-code]
  portable: true
allowed-tools: [Bash, Read]
---

# When to Use
...

# Prerequisites
...

# Procedure
...

# Pitfalls
...

# Verification
...
```

Standard Agent Skills frontmatter includes `name`, `description`, optional `license`, optional `compatibility`, optional `metadata`, and optional `allowed-tools`. Unknown fields are preserved for forward compatibility.

### 15.2 Frontmatter mapping

| Agent Skills field | Virtual OKF value | Requirement |
| --- | --- | --- |
| `name` | `title` and stable package name | Required, non-empty; `title` is derived from this field without mutating source. |
| `description` | `description` | Required and suitable for discovery. |
| `license` | metadata and optional OKF license policy | Optional string. |
| `compatibility` | metadata | Optional string describing environment constraints. |
| `metadata` | `metadata.agent_skill.metadata` | Optional mapping. |
| `allowed-tools` | `metadata.agent_skill.allowed-tools` | Optional declaration; it is not an execution authorization. |
| complete original frontmatter | `metadata.agent_skill` | Required preservation, including unknown fields and original scalar/array/object values. |

The virtual concept has `type: Skill`, `title: <name>`, a path-derived identity based on the package's `SKILL.md`, explicit visibility from package metadata or bundle policy default `internal`, and body equal to the Markdown after frontmatter. `metadata.agent_skill` MUST contain the complete parsed original frontmatter object, not only recognized keys. The mandatory `skills.skill_md` projection MUST preserve original `SKILL.md` bytes even when optional `concept_source` is disabled.

### 15.3 Body contract

The recommended first-level sections are:

1. `# When to Use` — triggers, intended tasks, and boundaries;
2. `# Prerequisites` — tools, access, inputs, and environmental assumptions;
3. `# Procedure` — ordered operational steps;
4. `# Pitfalls` — known failure modes, unsafe shortcuts, and recovery advice;
5. `# Verification` — observable checks that prove completion.

These headings are a pgokf quality profile, not a portability rewrite. Missing recommended sections produce diagnostics according to validation mode but do not invalidate an otherwise conformant Agent Skill unless local policy marks the profile strict. Additional headings are allowed and indexed. Markdown links are parsed for graph derivation as specified in §18.3.

## 16. Concept type: `Script`

A Script is a first-class, standalone executable concept. It can represent system-administration automation, CI/CD steps, operational runbooks expressed as code, glue code, maintenance utilities, or a helper shipped in a Skill package. It may come from any source adapter supported for Code Snippets—local filesystem, generic Git, GitHub, database/application content, or a discovered skill package. A Skill may link to a Script with `USES`, but a Script does not require a parent Skill and is independently addressable, searchable, retrievable, versioned, and trusted.

PostgreSQL never executes a Script during sync, search, retrieval, or validation. Execution, if offered, belongs to an authorized external runner and is governed by §20.4.

### 16.1 Canonical standalone document

````markdown
---
type: Script
title: Check PostgreSQL replication lag
description: Exits nonzero when a replica exceeds the configured lag threshold.
language: bash
runtime:
  executable: /usr/bin/env bash
  minimum_version: "4.0"
  requires: [psql]
arguments:
  - name: dsn
    position: 1
    type: string
    required: true
  - name: max_lag_seconds
    flag: --max-lag-seconds
    type: integer
    default: 30
exit_codes:
  0: replica is within the threshold
  1: replica exceeds the threshold
  2: invalid arguments or connection failure
tags: [postgresql, replication, monitoring]
visibility: internal
source:
  kind: github
  repository: https://github.com/acme/operations.git
  revision: 2dd9c90
  path: scripts/check-replication-lag.sh
---

# Schema

Argument and exit-code definitions are authoritative from frontmatter; this section
provides human-readable details and operational constraints.

# Script

```bash
#!/usr/bin/env bash
set -euo pipefail
# ...
```

# Examples

```console
./check-replication-lag.sh "$DATABASE_URL" --max-lag-seconds 30
```

# Pitfalls

Requires a role that can read PostgreSQL replication statistics.
````

The outer four-backtick fence is documentation framing only.

### 16.2 Frontmatter

| Field | Required | Rules |
| --- | --- | --- |
| `type` | yes | Exact value `Script`. |
| `title` | yes | Non-empty display name; package-derived virtual concepts may derive it from a sidecar or filename. |
| `description` | yes | Compact statement of what the script does, its main effect, and important safety boundary. |
| `language` | yes | Canonical identifier such as `bash`, `python`, `ruby`, or `javascript`/`node`; normalized using §3.3. Language and runtime are distinct. |
| `runtime` | recommended | String or mapping describing interpreter, minimum/exact version, platform, and required commands/packages. It is descriptive and never grants execution authority. |
| `arguments` | recommended | Ordered argument/flag/environment-input declarations with name, type, position/flag, required/default, validation, and secret classification where applicable. Absence means unspecified, not zero arguments. |
| `exit_codes` | recommended | Mapping from integer exit status to documented meaning; duplicate or non-integer keys are invalid. |
| `source` | recommended | Same origin mapping as Code Snippet, including immutable revision where available. |
| `tags`, `author`, `license` | as policy requires | Same generic OKF and catalogue semantics as Code Snippet. |
| `visibility` | yes | `public`, `internal`, or `private`; package-derived Scripts inherit/narrow the Skill visibility. |

Recognized language aliases may include `sh → bash` only when local policy has verified Bash semantics; otherwise portable POSIX shell should remain `shell`. A Node.js runtime does not change JavaScript source language: use `language: javascript` and a runtime such as `{executable: node, minimum_version: "20"}`.

### 16.3 Body structure and exact source

A standalone Script body contains:

- exactly one first-level `# Schema` section describing arguments, environment inputs, outputs, side effects, and exit codes; frontmatter is authoritative where duplicated;
- exactly one first-level `# Script` section containing exactly one canonical fenced code block whose info string matches `language`;
- zero or one `# Examples` section with usage examples that are indexed at lower weight and never executed;
- zero or one `# Pitfalls` section describing destructive behavior, privilege needs, non-idempotence, portability limitations, and known failure modes.

The fenced `# Script` payload is the exact executable source and follows the byte-preservation rules of §3.4. Retrieval MUST return that exact payload rather than `body_text`. The validator compares shebang, declared language, and runtime when present and reports conflicts.

A raw executable discovered under a Skill package's `scripts/` directory is one ingestion form, not the definition of Script. pgokf synthesizes an equivalent virtual document/projection without modifying the raw file: title and description come from recognized package metadata or safe filename defaults; language/runtime come from explicit metadata, extension, and unambiguous shebang; the raw UTF-8 file becomes the exact `# Script` payload. Package-side metadata conflicts are diagnostics, and binary executables are not Script concepts in this version.

### 16.4 Ingestion, metadata, and independent search

Standalone `type: Script` OKF Markdown is accepted through every source path that accepts Code Snippets: filesystem bundles, Git/GitHub companion adapters, `register_bundle_content`, and safe record-at-a-time content authoring. Git/GitHub selection filters MUST include Script when configured and MUST not require a `SKILL.md` ancestor. Package-discovered raw scripts additionally use §18 ingestion.

Every Script populates the same generic `pgokf.concepts` columns as all other concepts and stores unknown/type-specific fields in `pgokf.concept_metadata`, including `language`, `runtime`, `arguments`, `exit_codes`, `source`, and package provenance when applicable. The mandatory `pgokf.scripts` projection MUST store exact Script bytes and typed declarations because `body_text` and optional source storage cannot guarantee executable bytes. It is the authoritative retrieval payload and is hash-bound to scanning/history; it remains part of the same catalogue.

Scripts participate directly in `pgokf.concept_search`. A caller may search all types, or pass `concept_type => 'Script'` to search scripts without first discovering or activating a Skill. Script ranking includes title/description/tags, argument and exit-code descriptions, exact source tokens, examples, and pitfalls with documented weights.

## 17. Concept type: `Reference`

A Reference is first-class factual/supporting material, never an instruction to execute. It may be a standalone OKF document from any filesystem, content, Git, GitHub, API, database, or object-store adapter, or a virtual artifact from a Skill `references/`/`assets/` path. It does not require a Skill ancestor.

### 17.1 Standalone Reference

```markdown
---
type: Reference
title: PostgreSQL failover constraints
description: Decision table and recovery safety constraints.
format: markdown
author: team:database-reliability
source:
  kind: git
  repository: https://example.invalid/runbooks.git
  revision: 2dd9c90
  path: failover.md
tags: [postgresql, failover]
visibility: internal
license: Apache-2.0
provenance:
  verified_by: human:alice
---

# Summary
...
```

`type`, `title`, `format`, `author`, and `visibility` are required. `description`, `source`, `tags`, `license`, and provenance/OKF `sources` are recommended and retain their normal meanings. `format` is a canonical lower-case format (`markdown`, `text`, `json`, `yaml`, `pdf`) or media type (`image/png`, `application/octet-stream`). Standalone Markdown body after frontmatter is the exact textual Reference payload and is stored both as exact UTF-8 bytes and indexed text; frontmatter is metadata, not part of `exact_bytes`. A standalone wrapper may reference an external immutable asset with a content digest, but ingestion MUST fetch/stage and verify exact bytes before creating an available projection; unresolved remote URLs are metadata only and cannot masquerade as stored content.

All ordinary source adapters classify this as `OkfDocument` and pass it through generic parsing, then the Reference projector. Content/Git/GitHub inputs MUST be complete snapshots under existing semantics. Binary standalone References use the record-at-a-time/package resource form with metadata supplied alongside exact bytes; they MUST NOT be coerced through Markdown.

### 17.2 Package-derived Reference

Every confined file below an owned `references/` or `assets/` path materializes a virtual Reference. Title derives from explicit sidecar/package metadata, then first heading for safe UTF-8 text, then filename. Description derives only from explicit metadata or bounded safe text extraction. Format/media type comes from explicit metadata plus verified magic bytes; extension disagreement is a diagnostic. Visibility inherits/narrows the owning Skill. `source_path`, package identity, exact bytes, byte size/hash, and whether it came from `references` or `assets` are mandatory.

UTF-8 text is indexed directly. Binary extraction/OCR is optional, bounded, scanner-versioned, marked derived, and never replaces the original. Exact retrieval uses `get_reference` (§8.6); binary/large values use capability-aware bounded streaming. Archive bomb rules in §11 apply before any extraction.

## 18. Skill package ingestion and relationship graph

### 18.1 Detection and materialization during bundle sync

For every complete raw bundle snapshot, after §5.4 classification and before generic parsing of resources, the synchronizer SHALL:

1. detect case-sensitive files named exactly `SKILL.md` after normal path normalization and confinement checks;
2. treat each containing directory as one skill package and parse Agent Skills YAML frontmatter plus Markdown body;
3. synthesize a virtual `type: Skill` concept whose title is derived from frontmatter `name` and whose identity remains bundle/path based;
4. preserve all original frontmatter under `metadata.agent_skill` and preserve source bytes when configured;
5. enumerate confined sibling files beneath `scripts/`, `references/`, and `assets/`, applying file-count, byte-size, symlink, encoding, and exclusion limits;
6. materialize textual executable helpers from `scripts/` as `type: Script` and materialize files from `references/` and `assets/` as `type: Reference`;
7. compute the §9 package hash and upsert generated concepts/mandatory projections/links/diagnostics in the same atomic transaction as ordinary concepts, provenance, history, and bundle hashes;
8. perform §9 package-level dependency invalidation and delete stale virtual artifacts, links, scans, and diagnostics when package membership/ownership changes or files cease to qualify.

Virtual identities MUST be deterministic and collision-free. The recommended IDs are the normalized bundle-relative source paths including the package directory, with the existing extension-removal convention applied only where unambiguous. A virtual artifact MUST retain its exact package-relative source path in metadata. If a generated ID collides with a file-authored OKF concept, strict mode aborts and warn mode reports the collision without silently replacing either concept.

`register_bundle_content` callers MUST include the complete skill package snapshot, including non-Markdown resource bytes. Filesystem and content `ByteSource` implementations therefore need resource enumeration beyond the current Markdown-only concept parser. Resource files are not parsed as standalone OKF documents before their virtual projections are constructed.

### 18.2 Link model and edge types

Relationships use the explicit `relationships` syntax and migration in §5.6 plus Markdown derivation; no parallel graph table is introduced. Each edge records source and target bundle/concept identity where resolvable, canonical relationship, raw target, resolution state, edge metadata, and complete derivation locations.

| Source | Edge | Target | Meaning |
| --- | --- | --- | --- |
| Skill | `USES` | Script | The procedure invokes or delegates to an executable helper. |
| Skill | `REFERENCES` | Reference | The skill loads or cites factual/supporting material. |
| Skill | `INSTANTIATES` | Scaffold Template | The procedure renders or creates from a scaffold. |
| Skill | `DEMONSTRATES` | Code Snippet | The skill uses a snippet as an example or implementation pattern. |
| Skill | `REQUIRES` | external `tool:` URI | The skill declares a prerequisite; Tool is not a concept type. |
| Skill | `RELATED_TO` | Skill | The skill cross-references another skill without a stronger semantic edge. |

Relationship names are case-sensitive canonical values. Unknown link relationships remain valid generic OKF links but do not receive these typed semantics. Links do not imply permission, trust, installation, or execution.

### 18.3 Derivation from Markdown links

The projector parses inline links, reference-style links, and autolinks in the `SKILL.md` body after Markdown syntax resolution. It ignores links inside code fences and inline code. A confined relative link is resolved against the package directory, percent-decoded only according to URI rules, normalized, and rejected if it traverses outside the bundle/package policy.

Derivation rules are deterministic:

- a target under `scripts/` becomes `USES`;
- a target under `references/` or `assets/` becomes `REFERENCES`;
- a resolved target concept of type `Scaffold Template` becomes `INSTANTIATES`;
- a resolved target concept of type `Code Snippet` becomes `DEMONSTRATES`;
- a resolved target concept of type `Skill` becomes `RELATED_TO`;
- structured prerequisite declarations and recognized `tool:`/`tool://` URIs become external `REQUIRES` edges; they never resolve to a `Tool` concept type.

Explicit `relationships` frontmatter (§5.6) MAY declare one of the canonical relationships and takes precedence over path inference when it resolves to the same target. An explicit/path conflict is a validation diagnostic rather than two contradictory edges. Unresolved local links are retained as unresolved links with diagnostics; external HTTP(S) citations remain provenance/resource links unless an adapter maps them to a known concept. Duplicate derived edges are coalesced while retaining all derivation locations.

A sibling artifact need not be linked in prose to be indexed. The ingestor SHALL create package-membership links from the Skill to discovered artifacts: `USES` for scripts and `REFERENCES` for references/assets. Markdown-derived occurrences enrich those edges with source locations.

## 19. Unified search and progressive discovery

### 19.1 One ranked search surface

All five concept types participate in both backward-compatible `pgokf.concept_search` and the enriched normative `pgokf.catalogue_search` of §7.2. `concept_type` remains an optional exact filter:

- omitted or `NULL`: search every concept type;
- `Skill`: search skills only;
- `Code Snippet`: search snippets only;
- `Script`, `Reference`, or `Scaffold Template`: search that type only.

Enriched ranking is computed in one result set using the exact `catalogue-rank-1` formula and cursor semantics in §7.2, not by concatenating separately paginated type queries. The legacy function keeps its existing ranking contract.

For example, a single search for `PostgreSQL failover` can return, in rank order:

1. a `Skill` describing the failover procedure;
2. a `Code Snippet` implementing a replication-state check;
3. a `Script` that performs the check;
4. a `Reference` containing recovery constraints.

Each result includes at least identity, title, description, type, tags, rank, headline, lifecycle/trust summary, and a resource-availability summary. Exact script/reference bodies are never embedded in generic search results.

### 19.2 Three-stage progressive disclosure

Agent Skills progressive disclosure maps to a stable UI/API contract:

1. **Discovery:** return a compact index containing `name`/title, description, type, tags, identity, and compact trust/portability indicators. The response SHOULD target approximately 100 tokens per concept and MUST omit full bodies and artifact bytes.
2. **Activation:** when a user or agent selects a Skill, fetch its full body, original Agent Skills metadata, provenance, validation diagnostics, and relationship summaries.
3. **Resource access:** fetch linked Script or Reference content only when the procedure reaches that resource or the user explicitly opens it. Binary assets use bounded streaming/download endpoints rather than generic JSON embedding.

The UI MUST preserve stable `(bundle_id, concept_id)` identities between stages. Discovery responses expose typed links as counts and compact targets; activation may expand the relationship graph; resource retrieval requires a separate authorized request. Clients MUST NOT infer that a discovery result has been activated, that a linked script is safe to run, or that all resources should be preloaded.

Recommended UI-facing discovery shape:

```json
{
  "identity": {"bundle_id": 12, "concept_id": "skills/postgresql-failover/SKILL"},
  "name": "postgresql-failover",
  "description": "Diagnose and recover a PostgreSQL primary failure safely.",
  "type": "Skill",
  "tags": ["postgresql", "failover"],
  "supported_clients": ["hermes-agent", "claude-code"],
  "portable": true,
  "trust": {"status": "verified", "score": 0.91},
  "resources": {"scripts": 1, "references": 2}
}
```

## 20. Trust, portability, and execution safety

### 20.1 Client support and portability

Skill metadata SHOULD record:

```yaml
metadata:
  supported_clients:
    - hermes-agent
    - claude-code
  portable: true
```

`metadata.agent_skill.metadata.supported_clients` is an ordered or set-like list of declared compatible client identifiers. `portable` is a boolean producer assertion validated by pgokf; it is not inferred merely because both named clients can read `SKILL.md`. The derived generic metadata MAY project these values to indexed convenience keys, but the preserved original remains authoritative for what the producer declared.

A portable Skill uses standard Agent Skills fields, relative package resources, client-neutral procedure text, and no undeclared vendor-only tools or paths. A non-portable Skill remains ingestible and searchable but MUST carry structured diagnostics explaining why, such as:

- `client_specific_field` — required semantics depend on an extension field;
- `unsupported_tool` — an allowed/required tool is unavailable in one or more declared clients;
- `absolute_resource_path` or `external_resource_dependency`;
- `runtime_incompatible` — linked Script runtime is unsupported by a declared client;
- `missing_resource`, `resource_escape`, or `frontmatter_conflict`;
- `portable_assertion_mismatch` — `portable: true` conflicts with detected requirements.

Diagnostics include code, severity, concept identity, source location when available, affected clients, and remediation. Strict local policy MAY reject a false portability assertion; otherwise ingestion retains the concept with `portable: false` in the derived view and preserves the producer's original assertion separately.

### 20.2 Provenance and trust scoring

Trust uses existing OKF provenance and lifecycle data rather than package location or popularity. At minimum the trust view considers:

- `generated`: whether content or derived text was machine-generated and by which actor/process;
- `verified`: verification event, verifier identity, method, and timestamp;
- `sources`: resolvable source/derivation records and immutable revisions where available;
- `status`: lifecycle state such as draft, active, deprecated, or retired;
- `stale_after`: time after which freshness-dependent claims require re-verification.

A deployment MAY compute a numeric trust score for ranking/display, but MUST also return the underlying factors and policy/version that produced it. Generated content is not automatically untrusted, verified content is not permanently trusted, and stale or retired concepts are downgraded or filtered according to explicit policy. Trust of a Skill does not automatically transfer to a mutable linked Script or Reference; each target retains its own file hash and provenance, and aggregate Skill trust MUST account for required artifacts.

### 20.3 Validation output

Skill/package validation reports separate dimensions: format validity, portability, client compatibility, resource integrity, link integrity, provenance freshness, and script security. A compact discovery result exposes only status/counts; activation exposes full diagnostics. Diagnostics are deterministic for the same bundle snapshot and validator policy/version and are stored or reproducible without mutating portable source.

### 20.4 Script security before execution

PostgreSQL and pgokf never execute Script concepts. Any external client or runner that offers execution MUST, after exact retrieval and before every execution:

1. verify bundle/concept identity and expected `file_hash` so scan and execution refer to identical bytes;
2. enforce tenant/visibility authorization and local trust policy;
3. run configured static security scanners and malware/secret checks appropriate to `language`/`runtime`;
4. validate declared arguments without shell-string interpolation and reject undeclared secrets;
5. require explicit user/policy authorization for capabilities such as network, filesystem writes, credential access, privilege escalation, subprocesses, and package installation;
6. execute in a sandbox with least privilege, resource/time limits, controlled environment, and auditable inputs/outputs;
7. refuse execution on scan failure, stale scan results, unsupported runtime, hash mismatch, unresolved required resources, or policy denial.

Scan results and findings use the mandatory §11.1 schema, status/severity vocabulary, signature state, exact-hash binding, expiry, and restricted details. A clean scan is evidence for one immutable byte sequence, not a guarantee or execution authority. §7 excludes unsafe Scripts by default and labels/demotes them only for explicitly authorized all-status searches; restricted findings never appear in headlines.

## 21. References

- Open Knowledge Format v0.2, `SPEC.md`: <https://github.com/GoogleCloudPlatform/open-knowledge-format/blob/main/SPEC.md>
- pgokf parser model: `/datapool/projects/okf-pg-catalog/crates/okf-parser/src/model.rs`
- pgokf catalogue schema: `/datapool/projects/okf-pg-catalog/crates/extension/src/catalog/schema.rs`
- pgokf search implementation: `/datapool/projects/okf-pg-catalog/crates/extension/src/catalog/search.rs`
- pgokf synchronization engine: `/datapool/projects/okf-pg-catalog/crates/extension/src/catalog/sync.rs`
- pgokf exact-source projection: `/datapool/projects/okf-pg-catalog/crates/extension/src/catalog/source.rs`
- pgokf SQL API: `/datapool/projects/okf-pg-catalog/docs/sql-api.md`
- Standalone Code Catalogue and CTL1 specification: `/datapool/projects/code-catalogue/SPEC.md`
- Agent Skills specification: <https://agentskills.io/>

---

This extension preserves the OKF principle that concept types are open and documents remain readable. The database-specific value is a validated, tenant-safe, searchable projection, a relationship graph, progressive resource access, and a deterministic non-executing renderer; the portable sources of truth remain OKF Markdown, Agent Skills packages, and their original artifact bytes.