# OKF starter templates

Copy-and-fill starting points for authoring your own OKF Markdown concepts and
registering them with the `pgokf` extension. Each file here is a **valid OKF
concept**: YAML frontmatter delimited by `---` lines, then a Markdown body.

## Templates

| File | `type` | What it models |
| ---- | ------ | -------------- |
| [`runbook.md`](runbook.md) | `runbook` | An operational procedure with preconditions and steps. |
| [`wiki-article.md`](wiki-article.md) | `article` | A knowledge / wiki article. |
| [`service.md`](service.md) | `service` | A service, its owner, dependencies, and runbooks. |
| [`incident.md`](incident.md) | `incident` | An incident / postmortem record. |
| [`skill.md`](skill.md) | `skill` | A competency, with producer-extension metadata keys. |
| [`index.md`](index.md) | _(reserved)_ | A bundle-root `index.md` carrying `okf_version` — **not** a concept. |

The concept templates cross-link each other (the incident points at the runbook
and the service; the skill points at all three), so a bundle assembled from
them exercises the link graph out of the box. `index.md` is a reserved OKF file
(see below), not one of the cross-linked concepts.

## Use them

1. **Copy** one or more templates into a bundle directory (any folder of `.md`
   files), renaming as you like:

   ```bash
   mkdir -p /abs/path/to/my-bundle
   cp templates/service.md /abs/path/to/my-bundle/payments-api.md
   ```

   The concept **id is derived from the file path** (bundle-relative, without
   `.md`) — e.g. `payments-api.md` → `payments-api`, `runbooks/failover.md` →
   `runbooks/failover`. Any `id:` you put in frontmatter is recorded for
   diagnostics but never used as the key. The filenames `index.md` and `log.md`
   are reserved and are **not** ingested as concepts.

2. **Fill in** the frontmatter and body. Delete the `<!-- ... -->` guidance
   comments (they are inert, but there is no reason to keep them).

3. **Register** the bundle with `pgokf` (requires the `pgokf_admin` role):

   ```sql
   SELECT * FROM pgokf.register_bundle('/abs/path/to/my-bundle', 'payments');
   ```

   Re-run after edits with `SELECT pgokf.refresh_bundle(<bundle_id>);`, and list
   what is registered with `SELECT * FROM pgokf.list_bundles();`.

4. **Query** it (requires the `pgokf_reader` role):

   ```sql
   SELECT * FROM pgokf.concept_search('payments restart');
   SELECT * FROM pgokf.concept_neighbors('payments-api', 2);
   ```

## How frontmatter maps to the catalog

| Frontmatter | Where it lands |
| ----------- | -------------- |
| `type`, `title`, `description` | Columns on `pgokf.concepts`. |
| `tags` (list) | One row per tag in the tag projection (searchable / filterable). |
| `resource` | Stored verbatim (as JSON) on `pgokf.concepts.resource`. |
| Markdown links `[label](target.md)` | Rows in `pgokf.links`. Internal targets resolve to other concepts; `http(s):` / `mailto:` targets are flagged external. |
| **Every non-modeled key** (anything other than `type`/`title`/`description`/`tags`/`resource`) | One row per key in `pgokf.concept_metadata` (key + value as JSON text). |
| OKF v0.2 provenance / trust / lifecycle keys | **Additionally** projected into `pgokf.concept_provenance` and its child tables `pgokf.concept_verification` / `pgokf.concept_provenance_source` (see below). These keys are not removed from `concept_metadata` — they appear in both places. |
| Bundle-root `index.md` `okf_version` | Read from the reserved bundle-root `index.md` and stored on `pgokf.bundles.okf_version`. `index.md` itself is **not** a concept. See [`index.md`](index.md). |

### Provenance / trust / lifecycle (OKF v0.2)

The OKF v0.2 PROVENANCE, TRUST, and LIFECYCLE families are lifted into typed
columns across three tables; the recognized key set is also retained losslessly
as JSON. A concept carrying **none** of these keys gets no provenance row at all
(the projection is sparse).

**`pgokf.concept_provenance`** — one scalar row per provenance-bearing concept:

| Column | Sourced from frontmatter |
| ------ | ------------------------ |
| `generated_by` | `generated.by` (tolerates a bare `generated_by`) — the actor that produced the current content |
| `generated_at` | `generated.at`, ISO 8601 (tolerates a bare `generated_at`) |
| `status` | `status` (`draft` \| `stable` \| `deprecated`; spec default when absent is `stable`) |
| `stale_after` | `stale_after`, an absolute ISO 8601 instant |
| `usage_window_from` / `usage_window_to` | the top-level `usage_window {from, to}` |
| `trust_tier` | **derived** from the `verified[]` actors: `human-reviewed` if any actor is `human:`, else `machine-confirmed` with ≥1 event, else `unverified` |
| `details` | lossless `jsonb` copy of the recognized provenance/trust/lifecycle keys |

**`pgokf.concept_verification`** — one row per `verified[]` event (`ordinal`,
`verified_by` actor, `verified_at`). `verified` is an ordered **list** of
`{by, at}` events; a single mapping is stored as one 0-ordinal row. Omit
`verified` entirely for an unverified draft.

**`pgokf.concept_provenance_source`** — one row per `sources[]` entry
(`ordinal`, `source_id`, `resource`, `title`, `author`, `usage_count`,
`last_modified`, and per-entry `usage_window_from` / `usage_window_to`). These
are the provenance materials the content was derived from, distinct from the
raw-bytes `pgokf.concept_source` table.

Actors use the OKF convention: an agent is `<producer>/<version>` (e.g.
`sre-agent/1.4`), a person is `human:<id>`, a process is `process:<id>`.

### Custom metadata (`pgokf.concept_metadata`)

**Every** non-modeled frontmatter key is kept verbatim here as producer
metadata — this includes the provenance keys above (they land in
`concept_metadata` *and* the provenance tables). See [`skill.md`](skill.md) for
purely-custom producer **extensions** (`owner`, `proficiency_levels`,
`required_for_role`, `review_cadence_days`) — OKF defines no skill-specific
fields, so these appear only in `concept_metadata`.

## Reference

Only the functions above (`register_bundle`, `refresh_bundle`,
`unregister_bundle`, `list_bundles`, `bundle_info`, `concept_search`,
`concept_neighbors`) and the `pgokf.*` tables are part of the supported SQL
surface. See `docs/sql-api.md` in this repository for the full, authoritative
API.
