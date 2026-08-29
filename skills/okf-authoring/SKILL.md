---
name: okf-authoring
description: Use when writing or editing Open Knowledge Format (OKF) concept .md files that must register cleanly into the pgokf catalog - getting the YAML frontmatter contract right (type/title/description/tags/resource + provenance keys), understanding the path-derived concept ID, building the link graph with Markdown links, and avoiding reserved filenames.
---

# Authoring OKF concepts

An OKF concept is **one `.md` file**: a YAML frontmatter block delimited by
`---` lines, followed by a Markdown body. The `pgokf` catalog parses these files
and projects them into PostgreSQL. This skill is the authoring contract, grounded
in the real parser (`crates/okf-parser/src/`). Start from the ready-made files
in the `templates/` directory and adapt them.

## File shape

```markdown
---
type: Runbook
title: Database failover
description: How to fail the primary over to a replica.
tags: [oncall, postgres, incident]
resource: https://runbooks.example.test/db-failover
---

# Database failover

Body prose in Markdown. Link to [the PostgreSQL service](../services/postgresql.md)
to build a graph edge.
```

Rules the parser enforces:

- The file **must** begin with `---` on its own first line (a single leading
  UTF-8 BOM is tolerated). A missing opening delimiter is an error.
- The frontmatter block ends at the **first** line whose entire content is `---`.
  Because the split is line-based, never put a bare `---` on its own line inside
  a multiline quoted value - it closes the block early. Use a single line or a
  block scalar (`|` / `>`) instead.
- The body is everything after the closing `---`.

## Frontmatter contract

Modeled fields (typed columns in `pgokf.concepts`):

| Key | Required | Type | Notes |
| --- | -------- | ---- | ----- |
| `type` | **yes** | string | OKF concept type, e.g. `Runbook`, `Reference`, `Skill`. Free-form. |
| `title` | **yes** | string | Concept title. Weighted highest in search. |
| `description` | no | string | Short summary. |
| `tags` | no | list of strings | Declaration order preserved; queryable as `text[]`. |
| `resource` | no | any YAML | A URL or structured value; stored as JSON text. |

`type` and `title` are the only required keys - omitting either makes the file
unparseable, and because sync is strict, one bad file aborts the whole bundle
registration (`22023`). Any **other** key you add is preserved verbatim as
metadata (see below); nothing is silently dropped.

Minimal valid concept:

```markdown
---
type: Reference
title: PostgreSQL service
---

Body text.
```

## The path-derived ID rule

The concept ID is **derived from the file path**, not from frontmatter: it is the
normalized bundle-relative path with the `.md` suffix removed.

| File path within bundle | Concept ID |
| ----------------------- | ---------- |
| `services/postgresql.md` | `services/postgresql` |
| `runbooks/database-failover.md` | `runbooks/database-failover` |
| `index-of-terms.md` | `index-of-terms` |

You **may** put an `id:` in frontmatter, but it never becomes the catalog key -
it is captured as `declared_id` for diagnostics only (e.g. duplicate-id reports).
The path always wins. To control a concept's ID, name and place its file
deliberately. Keep paths relative and traversal-free (no leading `/`, no `..`);
files must have a `.md` extension.

## Reserved filenames

`index.md` and `log.md` are reserved at **every** directory level - they carry
bundle/directory bookkeeping and are **not** concepts. The catalog never turns
them into concept rows (it reads them only for their reserved bookkeeping, see
below) and rejects them if parsed directly. Do not author concept
content in a file named `index.md` or `log.md`; name it something else (e.g.
`overview.md`). Note `reindex.md` or `catalog.md` are fine - only the exact
names `index.md`/`log.md` are reserved.

The **bundle-root `index.md`** may carry ONLY an optional `okf_version` in its
frontmatter; the catalog reads it and stores it on `pgokf.bundles.okf_version`
for that bundle. Both `okf_version: "0.2"` and unquoted `okf_version: 0.2` are
accepted; an absent or malformed value simply leaves the column `NULL`. A
declared but unsupported version (the build supports 0.2 / 0.2.x) is warned
about and indexed anyway by default, or rejected with `22023` when the catalog's
`okf_version_policy` config key is set to `reject`. `log.md` is the
chronological bundle/directory history: each non-blank line becomes one
`pgokf.bundle_log` entry (a leading ISO 8601 timestamp is lifted into
`logged_at`), readable via `pgokf.list_bundle_log`. Everything else in a
reserved file is ignored by the parser. The current format is **OKF v0.2**.

## Building the link graph

Markdown links become directed edges in `pgokf.links`, and `concept_neighbors`
traverses them. Write ordinary inline links; the target is resolved relative to
the linking file's directory:

```markdown
See [the failover runbook](../runbooks/database-failover.md).
Link to a [sibling](appendix.md) or from the [bundle root](/services/postgresql.md).
```

Resolution rules:

- A destination starting with `/` resolves from the **bundle root**; anything
  else resolves from the **current file's directory** (`./`, `../` supported).
- A destination with **no extension** gets `.md` appended (`[x](sibling)` →
  `sibling.md`). Non-Markdown extensions (`.png`, `.pdf`) never become concept
  edges.
- Fragments (`#section`) are stripped for resolution; a fragment-only link
  (`[top](#intro)`) points at the file itself.
- An edge is **resolved** only when the target concept actually exists in the
  same bundle. Broken internal links are still recorded (OKF permits them) but
  are not traversed by `concept_neighbors`.
- `http:`/`https:`/`mailto:` and protocol-relative (`//host/...`) destinations
  are flagged **external** and never become internal graph edges. Email
  autolinks (`<a@b.test>`) count as external too.

To make two concepts graph-adjacent, link one to the other by its **relative
`.md` path**, and ensure both files are inside the same registered bundle.

One frontmatter family also produces edges: on a concept whose `type` is
`Attested Computation`, the `computation` / `executor` / `attester` reference
fields (a bare resource path, or a `{resource: ...}` mapping) resolve like body
links into typed `pgokf.links` edges (`link_relation` =
`attestation:computation` / `attestation:executor` / `attestation:attester`),
so `concept_neighbors` can reach those concepts even when the body never links
them. No other type gets frontmatter-derived edges.

## Provenance, trust, and lifecycle metadata (OKF v0.2)

Any frontmatter key beyond the five modeled fields is retained losslessly in
`pgokf.concept_metadata` (one row per key, as `jsonb`). The OKF v0.2
PROVENANCE, TRUST, and LIFECYCLE families are **additionally** projected into
three typed tables. Use the real field shapes below - not a `generated_by`
scalar, a `verified` bool, `verification_method`, or `freshness`, none of which
are OKF fields.

**Actor convention.** Every actor is written as `<producer>/<version>` for an
agent (e.g. `reference_agent/gemini-2.5-pro`), `human:<id>` for a person, or
`process:<id>` for an automated process.

| Family | Frontmatter | Projects into |
| ------ | ----------- | ------------- |
| TRUST - origin | `generated: { by: <actor>, at: <ISO 8601> }` | `concept_provenance.generated_by` / `generated_at` |
| TRUST - verification | `verified: [ { by: <actor>, at: <ISO 8601> }, … ]` (a LIST of events; a single mapping counts as one) | one `concept_verification` row per event; the derived `concept_provenance.trust_tier` |
| LIFECYCLE | `status: draft \| stable \| deprecated` (default `stable`); `stale_after: <ISO 8601>` | `concept_provenance.status` / `stale_after` |
| PROVENANCE - usage | top-level `usage_window: { from, to }` | `concept_provenance.usage_window_from` / `usage_window_to` |
| PROVENANCE - sources | `sources: [ { resource, id, title, author, usage_count, last_modified, usage_window } ]` (`resource` is the only per-entry required key) | one `concept_provenance_source` row per entry |

`trust_tier` is **derived** from the verification events: `human-reviewed` as
soon as any `verified[].by` is a `human:` actor, else `machine-confirmed` with
at least one event, else `unverified`. Omit the `verified` block entirely for an
unverified draft - never write a bare `verified: true`. Every recognized key is
also kept verbatim in the `concept_provenance.details` jsonb. A concept carrying
none of these keys gets no provenance row at all (the projection is sparse).

Example header carrying the full shape:

```markdown
---
type: Attested Computation
title: Monthly active accounts
description: Canonical monthly active-account computation.
tags: [analytics, accounts, monthly]
status: stable
stale_after: 2027-01-01T00:00:00Z
generated:
  by: catalog-agent/1.0
  at: 2026-07-01T12:00:00Z
verified:
  - by: process:metric-validation
    at: 2026-07-02T02:00:00Z
  - by: human:fixture-reviewer
    at: 2026-07-03T09:30:00Z
usage_window:
  from: 2026-06-01T00:00:00Z
  to: 2026-06-30T23:59:59Z
sources:
  - id: account-policy
    resource: https://docs.example.test/policies/active-account
    title: Active account policy
    author: human:data-governance
    usage_count: 4200
    last_modified: 2026-06-15T08:00:00Z
---

# Definition

Body prose, with links to [the computation](/computation.md).
```

This projects `generated_by = catalog-agent/1.0`, `generated_at =
2026-07-01T12:00:00Z`, `status = stable`, `stale_after = 2027-01-01T00:00:00Z`,
a `usage_window`, and `trust_tier = human-reviewed` (a `human:` actor verified
it) into `concept_provenance`; two `concept_verification` rows; and one
`concept_provenance_source` row - with the full structures preserved in
`details`. Malformed values (an unparseable timestamp, a wrong-typed field)
degrade to `NULL` and never abort the sync.

### A note on OKF types

`type` is free-form and required; consumers tolerate unknown types. The **only**
spec-defined type with type-specific fields is **`Attested Computation`**
(`runtime` required, plus `parameters`, `computation`, `executor`, `attester`;
the last three become typed graph edges, see "Building the link graph").
Every other type - `runbook`, `service`, `wiki`/`article`, `incident`, `skill`,
… - is producer-defined with no OKF-prescribed fields: a "skill" is just
`type: skill` plus the recommended/provenance fields and any producer
extensions. Those extension keys are preserved in `concept_metadata`, never
mandated by OKF.

## Pre-registration checklist

- [ ] File starts with `---` and has a closing `---`; no stray `---` inside a
      multiline value.
- [ ] `type` and `title` are present and non-empty.
- [ ] Filename is not `index.md` / `log.md`; path is relative, `.md`, no `..`.
- [ ] Internal links use relative `.md` paths to files in the same bundle.
- [ ] Extra/provenance keys are valid YAML (they round-trip to JSON).

Real examples to copy: the `templates/` directory, `examples/sample-bundle/`,
and the metadata-rich fixtures in `tests/bundles/rich-metadata/`. To register and
query the resulting bundle, use the `pgokf-catalog` skill.
