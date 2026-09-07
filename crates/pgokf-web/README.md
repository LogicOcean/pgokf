# pgokf-web

The pgokf catalog's web UI and JSON API: search, browse, inspect, and monitor
a catalog through the `pgokf_reader` role. A thin, read-only companion that
holds no catalogue semantics of its own; every list, rank, filter, and
visibility decision is the database's.

## What it shows

| Page | Backed by |
| ---- | --------- |
| **Overview** (`/`) | `health()`, `search_index_status()`, `catalog_stats()`, `list_sync_log()` |
| **Search** (`/search`) | `concept_search()` (lexical, on the configured backend), `concept_search_semantic()` / `concept_search_hybrid()` when an embeddings endpoint is configured, `search_facets()` for the type, tag, bundle, status, and trust-tier facets, keyset pagination ("Load more"). With filters but no query text it **browses**: every visible concept matching the filters, so a tag, type, status, or bundle link lists concepts directly. |
| **Bundles** (`/bundles`, `/bundles/{id}`) | `catalog_stats()`, `list_bundles()`, the bundle's concepts grouped by directory, `list_sync_log()`, `list_bundle_log()` |
| **Concept** (`/concepts/{bundle_id}/{concept_id}`) | the concept row with its rendered Markdown (the stored source when `store_source` is on, otherwise the search text; body links to other concepts are resolved through `pgokf.links`, headings get anchor ids), custom metadata, provenance / verification / sources, outgoing and incoming links (folded per counterpart) with an interactive 3D link graph over `concept_neighbors()` and `pgokf.links` (rotate, zoom, click a node for details, explore onward from any node; a server-rendered SVG stands in without WebGL or JavaScript), `find_similar()`, `concept_history()`, and a source download through `get_concept_source()`; a skill package (`type: Skill`) gets a **Package** tab listing its scripts, references, and assets with sizes and SHA-256s, a script or reference shows which package it belongs to, and their exact bytes download through `get_skill()` / `get_script()` / `get_reference()` (`/resource/{bundle_id}/{concept_id}`) |
| **Graph** (`/graph`) | the catalog-wide link graph: the best-connected concepts of the catalog or one bundle (coloured by bundle or type) or, seeded from a concept, its neighborhood; nodes and edges open detail cards, zoom, center, 2D/3D, and full-screen controls, a finder, and "explore from here" |
| **Agent Plugin builder** (`/plugins`) | the agent plugin builder over the `pgokf-workspace` crate, laid out as five steps: choose the agent (a portable Agent Plugins 1.0.0 directory, Claude Code, Codex, Hermes Agent, Kimi, Gemini CLI, Cursor, the generic `.agents/` directory, an `AGENTS.md` instruction file, an Ollama prompt bundle, or a generic tree), choose the content (bundle, search, one-click type and tag chips from the catalog's facets, a file picker that browses a bundle and ticks specific files down to one script or reference of a package, and more selectors: concept ids, trust, size), add extras (the harness's MCP server entry, a catalog guide, an `okf.sh` helper over the JSON API), name it, then preview and download; the preview updates as you go, skill packages stored in the catalog are copied whole and byte for byte (marked in the preview with every file listed), and the equivalent `build_workspace_plugin` MCP call is one click away |
| **Operations** (`/status`) | `health()`, `search_index_status()`, `get_config()`, `stale_concepts()`, `duplicate_concepts()`, the sync log |
| **JSON API** (`/api/health`, `/api/bundles`, `/api/bundles/{id}/tree` (the picker's file list), `/api/search`, `/api/concepts/{bundle_id}/{concept_id}` (with `package` and `resource` for skill packages), `/api/graph?bundle=&limit=` and `/api/graph/{bundle_id}/{concept_id}?hops=N`) | the same data layer, for scripts and dashboards; the graph documents are what the 3D views draw |

Search results carry the database's `ts_headline` snippet (sanitized to its
highlight markup), the rank, the bundle, and the path. Facets, the result
list, and the page URL update in place as filters change (htmx); without
JavaScript the same forms and links do full page loads, and the concept
page shows its tabs as stacked sections. Every page is laid out for phones
as well: the navigation scrolls, tables scroll inside their panels, the
graph keeps its controls on the canvas, and a node's card lists its
connections so an edge can be reached by a tap.

## Configuration

| Flag | Environment | Meaning |
| ---- | ----------- | ------- |
| `--database-url` | `OKF_PG_URL` | Connection string for a `pgokf_reader` role (required). A writer or admin role is never needed. |
| `--bind` | `OKF_WEB_BIND` | Listen address (default `127.0.0.1:8080`). |
| `--tenant` | `OKF_TENANT` | `pgokf.tenant` scope applied to every pooled connection; required once the catalog's `require_tenant` policy is on. |
| `--tls` | `OKF_PG_TLS` | Force TLS to PostgreSQL. |
| `--pool-size` | `OKF_WEB_POOL_SIZE` | Pooled connections (default 8). |
| `--statement-timeout-ms` | `OKF_WEB_STATEMENT_TIMEOUT_MS` | Per-connection statement timeout (default 15000), bounding any single request. |
| `--embed-endpoint`, `--embed-model`, `--embed-api-key` | `OKF_EMBED_ENDPOINT`, `OKF_EMBED_MODEL`, `OKF_EMBED_API_KEY` | An OpenAI-compatible embeddings server; together they enable the semantic and hybrid search modes by embedding the query with the same model the catalog's vectors use. At startup the model's vector width is checked against the catalog's `embedding_dim` (a mismatch is a startup error); an endpoint that does not answer only logs a warning, and searches fall back to lexical results with a notice until it does. The modes are offered once the catalog has embedded concepts. |
| `--title` | `OKF_WEB_TITLE` | Header display name (defaults to the database name). |

Empty environment values are treated as unset, so a compose stack can pass
every variable unconditionally.

## Security model

- Visibility follows the extension: a retired or disabled bundle is invisible
  to search, browse, facets, and concept pages alike, and a concept another
  tenant owns is indistinguishable from one that does not exist. A body link
  the catalog could not resolve is shown as text, not as a dead link.
- Read-only by construction: the only role it needs is `pgokf_reader`, and it
  issues no statement outside the public `pgokf.*` API and the reader-visible
  projection tables. Tenant scope, visibility, `require_tenant`, and the
  non-disclosure rules are enforced by the database on the pooled role.
  Rendering a concept reads the stored source from the projection table;
  only the **Download source** button and a plugin download go through
  `get_concept_source()`, so the extension's access log records exports,
  not page views. A plugin preview reads no sources at all.
- No login of its own: it is the reader role's view of the catalog. Bind it
  to loopback (the default) and expose it through a reverse proxy that
  terminates TLS and authenticates users, or keep it on a private network.
- Catalog content is never trusted as HTML: Markdown bodies and search
  snippets pass through an allow-list sanitizer before rendering (no scripts,
  no event handlers, no `javascript:`/`data:` URLs, same-origin images only),
  the link graph is generated from escaped data, and every response carries a
  `Content-Security-Policy` that admits only same-origin scripts, styles, and
  images, plus `Referrer-Policy: same-origin`, `X-Frame-Options: DENY`, and
  `X-Content-Type-Options: nosniff`.
- Bounded work: a statement timeout on every connection, a 30 s bound on
  each request, a 5 s wait for a pooled connection (then 503), at most 64
  requests in flight, bundle listings paged by 500, and search pages capped
  at 100 rows. A page never holds a transaction.
- Failures are reported as pages or JSON with the right status: 400 for a
  value the catalog rejects, 404 for what the session cannot see, 502 when
  the embeddings service cannot serve the requested mode, 503 when the pool
  is exhausted, 504 on timeout. Under `/api/` every error is a JSON document
  `{"error": {"status", "message"}}`.

## Running

```sh
OKF_PG_URL=postgresql://okf_reader:...@db:5432/okf \
OKF_WEB_BIND=127.0.0.1:8080 \
pgokf-web
```

In the compose stack it is the `ui` profile:

```sh
docker compose --profile ui up -d
# http://127.0.0.1:8080 (PGOKF_UI_BIND / PGOKF_UI_PORT in .env)
```

## Development

```sh
cargo run -p pgokf-web -- --database-url postgresql://okf_reader:rd@127.0.0.1:5432/okf
cargo test -p pgokf-web
```

Templates live in `templates/` (Askama, compiled into the binary), styles and
scripts in `static/` (embedded). Vendored, licence alongside each:
`static/vendor/htmx.min.js` (htmx 2.0.4, Zero-Clause BSD) and
`static/vendor/3d-force-graph.min.js` (3d-force-graph 1.80.0 with its bundled
three.js, MIT). No build step and no runtime filesystem access are needed.
