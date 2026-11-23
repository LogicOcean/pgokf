---
name: pgokf-mcp
description: Use when an agent has pgokf catalog MCP tools (concept_search, get_concept, find_similar, concept_neighbors, traverse_concepts, get_skill, build_workspace_plugin, put_document) and must answer from the catalog, say how far a catalog document can be trusted, install a skill the catalog stores, build a plugin, or add a document - or when a catalog search returns nothing or only noise.
---

# Using a pgokf catalog through MCP

pgokf catalogs Open Knowledge Format (OKF) Markdown documents in PostgreSQL. Each
document is a **concept** identified by `(bundle_id, concept_id)`; the concept id
is its path inside the bundle without `.md`. A **bundle** is a named collection:
`filesystem` bundles are synced from a directory, `content` bundles are written
through this API.

What you can call depends on the server's version and your token's role
(`reader` < `builder` < `writer` < `admin`): use the tools your client lists. A
client may register several pgokf servers (`mcp__pgokf__…`, `mcp__pgokf-work__…`);
each is a different catalog. For SQL access to the database use the
`pgokf-catalog` skill; for the document format, `okf-authoring`.

## Searching

`concept_search` takes PostgreSQL websearch syntax:

| Query | Matches concepts with |
| --- | --- |
| `backup restore` | both words: every bare word must occur |
| `backup or snapshot` | either word |
| `"point in time"` | the phrase |
| `restore -docker` | `restore` and not `docker` |

- **Search with two or three distinctive words, not the question.** Every extra
  word is one more condition a document must meet.
- **An empty result means those words don't all occur together, not that the
  catalog lacks the topic.** Drop words, add synonyms with `or`, try other
  bundles, and read the hits whose titles fit. When you report that the catalog
  has nothing, say what you searched and read.
- **Scope searches a large bundle floods.** A code-knowledge bundle can hold
  thousands of concepts and swamp single-word queries. Pass `bundle_id`, an exact
  `type` (`Runbook`, `Guide`, `Reference`, `Skill`, …) or `tags` (all of them must
  be present). `list_bundles` names the bundles (writer and admin tokens);
  otherwise read the bundle ids off the hits.
- **`rank` orders hits within one query only.** Don't compare ranks across
  queries.
- **A hit's `headline` is an excerpt.** Read the concept with `get_concept`,
  passing `bundle_id` as well as `concept_id`, before you rely on it. The same
  concept id can exist in several bundles.
- **Related material:** `find_similar` ranks concepts like a given one;
  `concept_neighbors` follows its outbound links; `traverse_concepts`, where
  listed, walks typed relationships in either direction (links outbound only).

## Trust

Each concept has a **trust tier**, derived from who verified it, and a lifecycle
**status**:

| `trust_tier` | Meaning |
| --- | --- |
| `human-reviewed` | a person verified it |
| `machine-confirmed` | only automated checks verified it |
| `unverified` | nobody has |

`status` is `draft`, `stable` (the default) or `deprecated`.

These are the only values the `trust_tier` and `status` filters accept. Any other
value (`verified`, `trusted`, `reviewed`) matches nothing, so an empty search
filtered by it says nothing about the documents.

`get_concept` does not return the tier. To find it, repeat the query that found
the concept, with its `bundle_id`, filtered to `human-reviewed`, then
`machine-confirmed`, then `unverified`. Send these one at a time and stop at the
first whose results include the concept: that is its tier. In parallel, the three
results are easy to attach to the wrong tier. If a filtered search returns as
many hits as its `limit`, raise the limit or search the concept's title words, so
the concept cannot fall off the end. `get_skill` and `build_workspace_plugin`
report `trust_tier` directly.

When you answer from the catalog, cite each source by bundle name, concept id and
tier, for example `pgokf-docs/operations (bundle 2, human-reviewed)`. When
sources disagree, prefer the higher tier and say so. Repeat the query with
`status: "deprecated"` and flag any source it returns.

## Installing a skill the catalog stores

A skill package is a concept of type `Skill` whose id is its directory plus
`/SKILL`, such as `skills/okf-authoring/SKILL`. Find skills with
`concept_search` and `type: "Skill"`; an unfiltered search can rank guides on the
same subject above the skill.

Install with the builder, which returns the exact bytes with their hashes:

```json
{"target": "claude-code", "picks": ["3:skills/okf-authoring/SKILL"], "components": []}
```

- **Never pass `output_dir` to an HTTP server.** It names a directory on the
  server, and an HTTP server refuses it. Only a local stdio server writes there.
- **Over HTTP the files come back in `contents`:** path to text for UTF-8 files,
  path to `{"encoding": "base64", "data": …}` for binary ones. Write each file
  at its path, set the executable bit where `files[].executable` is true, and
  check each file's SHA-256 against `files[].sha256`. A tree over 1 MiB is
  refused inline: pick fewer files.
- **The tree holds more than the package:** an `okf-knowledge` index skill,
  `okf-workspace.yaml` and `okf-workspace.lock`. For just the skill, install its
  directory. Keep the lock if you will want to know when the catalog's copy
  changes: `check_workspace_plugin_freshness` takes its text.
- **`get_skill` is for reading a skill**, not installing one: its text arrives as
  a JSON string, and its `file_hash` is the catalog's own hash, not the SHA-256
  of the file.

## Building a plugin from a selection

`list_plugin_targets` names the targets. `build_workspace_plugin` selects with
`bundle_ids`, `query`, `types`, `tags`, `picks`, `seeds` or `all`, narrowed by
`verified_only` and `stale_policy`, and returns the tree as above. `mcp_url`
points the package at an HTTP endpoint, with the token referenced as
`OKF_MCP_TOKEN` rather than written. A selection that matches nothing fails;
some servers report that only as an internal error, so check the selection with
`concept_search` first.

## Writing documents (writer and admin tokens)

1. **Pick a `content` bundle** from `list_bundles`. A filesystem bundle changes
   only through its directory and `refresh_bundle`. If more than one content
   bundle could hold the document and the user has not said which, ask.
2. **Send a complete OKF document** to `put_document`: `---`, YAML frontmatter,
   `---`, Markdown body. Frontmatter needs `type` and `title`; add `description`,
   `tags` and the lifecycle key `status: draft`. The `path` ends in `.md` and
   becomes the concept id without it.
3. **Leave out `verified` and `generated`.** The server stamps `generated` with
   your token and sets aside any `verified` claim; a document claiming someone
   else produced it is refused. Verification is a person approving the document
   in the pgokf web UI. When asked to mark a document verified, write it
   unverified and tell the user it needs that approval.
4. **Links resolve only inside the same bundle**, as relative `.md` paths. Name
   a concept in another bundle in prose, by bundle and concept id. Never invent a
   URL.
5. **Write only what the catalog or the user supplied.** Where a step is
   unknown, leave a marked gap instead of guessing.
6. **Read it back** with `get_concept` to confirm.

`delete_document` removes one document from a content bundle. `set_bundle_state`
enables, disables, retires or restores a bundle, all reversibly; deleting a
bundle is not available through MCP.

## Common mistakes

| Mistake | Instead |
| --- | --- |
| Searching with the whole question | Two or three distinctive words; `or` for synonyms |
| "The catalog has nothing on X" after one empty search | Several narrower searches, then report what you tried |
| `trust_tier: "verified"` | `human-reviewed`, `machine-confirmed` or `unverified` |
| Calling documents unverified because a filtered search came back empty | Check the filter value, then rerun the search per tier |
| Sending the three tier searches in parallel | One at a time, stopping at the first that includes the concept |
| Answering from `headline` excerpts | `get_concept` with `bundle_id` |
| `output_dir` on an HTTP server | Write the inline `contents` yourself and check `files[].sha256` |
| Retyping `get_skill` text into a file | Install from `build_workspace_plugin` with `picks` |
| Writing `verified:` or `generated:` into a document | Leave both out; a person approves it in the web UI |
| Linking to another bundle or a guessed URL | Name it in prose by bundle and concept id |
