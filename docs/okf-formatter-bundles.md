# Formatter-produced bundles (OKF v1.0 profile)

> **Status: contract stage.** This page documents the *consumer-facing contract*
> for bundles produced by an OKF formatter/compilator toolchain (the `okf`
> tooling lives with the producers, not in this repo). It is additive to
> [Authoring OKF concepts](okf-authoring.md): everything there holds for
> hand-written bundles; this page adds the stricter, machine-verified shape
> that formatter output conforms to. pgokf ingests both identically — the
> extension does not special-case either origin — but formatter bundles carry
> additional frontmatter and guarantees that consumers may rely on.

## What a formatter-produced bundle guarantees

Bundles emitted by a formatter conforming to the OKF v1.0 profile are:

1. **Byte-identical on re-render.** No wall-clock timestamps, no host paths,
   no hostnames. Rendering the same inputs under the same contract version
   produces the same bytes. `document_hash` (SHA-256 over rendered bytes) is
   stable across machines and runs.
2. **Identity-stable.** A concept's `okf.concept_identity` is a SHA-256 over
   length-prefixed canonical fields (repository/project, path, kind, name,
   discriminator). Line numbers are never identity inputs. Content changes
   change the hash; layout-only changes never do.
3. **Validated before ingest.** A compiler pass (producer-side, CI-runnable)
   has already rejected: schema violations, unresolved same-bundle links,
   identity collisions, and 12 named malformed-bundle classes. pgokf's own
   strict parse remains the last line of defense and behaves identically.
4. **Fully-indexable.** Every frontmatter key maps to a catalog column or to
   typed metadata; embeddings are applied *after* registration by a companion
   embedder through `pgokf.set_concept_embedding`, never shipped in the bundle.

## Frontmatter: the v1.0 profile keys

All keys from OKF v0.2 authoring remain. Formatter bundles additionally carry
these keys in a canonical, fixed order (tools compute them; authors never
hand-write them):

| Key | Meaning | Where it lands |
|---|---|---|
| `status` | `draft \| stable \| deprecated` | `concept_metadata` |
| `generated.by` | pipeline attestation, e.g. `agent:okf-format/<version>` | `concept_metadata` |
| `sources[]` | origin URIs + revisions (`resource` required, `id` optional) | `concept_provenance` / `concept_source` |
| `okf.concept_identity` | content-independent identity digest | `concept_metadata` |
| `okf.content_hash` | per-content revision digest | `concept_metadata` |
| `relationships[]` | typed, namespaced edges (`<namespace>:<name>`, ≤128 chars) | `relationship` (fenced publication) |
| `profile` / `render_version` | contract versions | `concept_metadata` |

Constraints the formatter enforces before writing (and the compiler
re-verifies): `description` ≤ 200 chars; `tags` ≤ 16, deduplicated,
order-stable; `title` ≤ 200 chars; relationship targets resolve same-bundle
or are recorded as typed, unresolved-but-retained edges — never silently
dropped.

## Embedding contract for formatter bundles

Vectors are catalog state, not bundle content. A companion embedder streams
caller-computed vectors via `pgokf.set_concept_embedding(bundle_id,
concept_id, real[])`, keyed by `(content_hash, model)` with guarded-CAS
staleness: a stale embedding is cleared only when the read-back matches the
exact newest observed source generation, bundle generation, embedding model,
and dimension. Consumers may rely on: an `indexed_at`-present concept with a
matching embedding row is current for its stated model.

## Freshness

Formatter output is producer-attested content. Scheduling a refresh
(`pgokf.schedule_refresh`) never marks anything fresh; only a successful
`pgokf.mark_fresh` CAS (or the producer's publication cycle) establishes
`fresh`, per [Freshness semantics](operations.md#freshness).

## Exit codes (producer-side tooling)

For interoperability, formatter-family tooling documents its exit codes as:
`0` success; `1` refused, naming the failed legs in the output; `2` usage
error. These match the certifier and registration tools' convention.