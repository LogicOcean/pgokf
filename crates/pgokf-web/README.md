# pgokf-web

The pgokf catalog's web UI and JSON API: search, browse, inspect, and monitor
a catalog through the `pgokf_reader` role - read-only until a writer
connection and an identity mode are configured (see the human workflow
below). A thin companion that holds no catalogue semantics of its own;
every list, rank, filter, and visibility decision is the database's.

## What it shows

| Page | Backed by |
| ---- | --------- |
| **Overview** (`/`) | `health()`, `search_index_status()`, `catalog_stats()`, `list_sync_log()` |
| **Search** (`/search`) | `concept_search()` (lexical, on the configured backend), `concept_search_semantic()` / `concept_search_hybrid()` when an embeddings endpoint is configured, `search_facets()` for the type, tag, bundle, status, and trust-tier facets, keyset pagination ("Load more"). With filters but no query text it **browses**: every visible concept matching the filters, so a tag, type, status, or bundle link lists concepts directly. |
| **Bundles** (`/bundles`, `/bundles/{id}`) | `catalog_stats()`, `list_bundles()`, the bundle's concepts grouped by directory, `list_sync_log()`, `list_bundle_log()` |
| **Concept** (`/concepts/{bundle_id}/{concept_id}`) | the concept row with its rendered Markdown (the stored source when `store_source` is on, otherwise the search text; body links to other concepts are resolved through `pgokf.links`, headings get anchor ids), custom metadata, provenance / verification / sources, outgoing and incoming links (folded per counterpart) with an interactive 3D link graph over `concept_neighbors()` and `pgokf.links` (rotate, zoom, click a node for details, explore onward from any node; a server-rendered SVG stands in without WebGL or JavaScript), `find_similar()`, `concept_history()`, and a source download through `get_concept_source()`; a skill package (`type: Skill`) gets a **Package** tab listing its scripts, references, and assets with sizes and SHA-256s, a script or reference shows which package it belongs to, and their exact bytes download through `get_skill()` / `get_script()` / `get_reference()` (`/resource/{bundle_id}/{concept_id}`) |
| **Graph** (`/graph`) | the catalog-wide link graph: the best-connected concepts of the catalog or one bundle (coloured by bundle or type) or, seeded from a concept, its neighborhood; nodes and edges open detail cards, zoom, center, 2D/3D, and full-screen controls, a finder, and "explore from here" |
| **Agent Plugin builder** (`/plugins`) | the agent plugin builder over the `pgokf-workspace` crate, laid out as five steps: say what you are building (an Agent Plugin, a skills package, an instruction file, a prompt bundle, or generic files) and for which agent (a searchable list per kind: a portable Agent Plugins 1.0.0 directory, Claude Code, Codex, GitHub Copilot, Hermes Agent, Kimi, Gemini CLI, Cursor, the generic `.agents/` directory, `AGENTS.md`, Ollama, generic; a name that is not listed is added as a new agent, with its skills directory for a skills package), name it, choose the content (browse a bundle or all bundles, find files by name, title, type, or tag, tick files or whole directories, or take everything in the scope and narrow it by type, tag, ranked search, or concept ids; a selection bar shows every rule and ticked file as a removable chip), add extras (the agent's MCP server entry - a server it starts, or an HTTP `pgokf-mcp --http` endpoint it calls, whose bearer token is referenced and never written - a catalog guide, an `okf.sh` helper over the JSON API), then preview and download; the preview updates as you go, skill packages stored in the catalog are copied whole and byte for byte, and the equivalent `build_workspace_plugin` MCP call is one click away |
| **Operations** (`/status`) | `health()`, `search_index_status()`, `get_config()`, `stale_concepts()`, `duplicate_concepts()`, the sync log |
| **Sign in**, **Upload**, **Edit**, **Review** | the human workflow (see below), shown to people whose role allows each step: an upload form for Markdown documents into a content bundle, a full-document editor with a validating preview, a review queue and a review tab on each document (approve, or send back with a note) |
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
| `--database-url` | `OKF_PG_URL` | Connection string for a `pgokf_reader` role (required): everything the UI shows comes through it. |
| `--writer-url` | `OKF_PG_WRITER_URL` | Connection string for a `pgokf_writer` role, used only by the human workflow (upload, edit, review) and only for signed-in people whose role allows it. Without it those pages are off and the UI is read-only. |
| `--bundles-dir`, `--bundles-db-dir` | `OKF_WEB_BUNDLES_DIR`, `OKF_WEB_BUNDLES_DB_DIR` | The directory under which directory bundles are reachable from this process (mounted read-write), and the path the database server uses for the same directory when it differs. With it set, editors change documents of directory bundles in place: the file is written atomically, confined to the bundle (no `..`, no symbolic links), and the bundle is refreshed, so the directory stays the source of truth. Unset, such bundles are read-only in the UI. |
| `--auth` | `OKF_WEB_AUTH` | How people are identified: `none` (everyone is a viewer; the default), `oidc` (this site signs people in against an OpenID Connect provider), `header` (a trusted reverse proxy forwards the identity), or `users` (a local users file with a login form). |
| `--oidc-issuer`, `--oidc-client-id`, `--oidc-client-secret`, `--oidc-redirect-url` | `OKF_WEB_OIDC_ISSUER`, `OKF_WEB_OIDC_CLIENT_ID`, `OKF_WEB_OIDC_CLIENT_SECRET`, `OKF_WEB_OIDC_REDIRECT_URL` | `oidc` mode: the provider's issuer URL exactly as it declares it, the client this site is registered as, its secret (omit it for a public client, which PKCE alone protects), and this site's callback URL, which is its public address plus `/auth/callback` and must be registered with the provider. |
| `--oidc-scopes`, `--oidc-subject-claims`, `--oidc-groups-claim`, `--oidc-provider-name` | `OKF_WEB_OIDC_SCOPES`, `OKF_WEB_OIDC_SUBJECT_CLAIMS`, `OKF_WEB_OIDC_GROUPS_CLAIM`, `OKF_WEB_OIDC_PROVIDER_NAME` | The scopes to ask for (default `openid profile email`; `openid` is always added), the claims tried in order for the person's identity (default `preferred_username,email,sub`), the claim carrying their groups (default `groups`), and what the sign-in button calls the provider. Roles come from `--auth-role-map` and `--auth-default-role`, and the session from `--session-secret` / `--session-hours` / `--cookie-secure`, exactly as in `users` mode. |
| `--auth-users-file`, `--session-secret`, `--session-hours`, `--cookie-secure` | `OKF_WEB_AUTH_USERS_FILE`, `OKF_WEB_SESSION_SECRET`, `OKF_WEB_SESSION_HOURS`, `OKF_WEB_COOKIE_SECURE` | `users` mode: the file (`name:role:$argon2id$...` per line, made with `pgokf-web hash-password --user NAME --role ROLE < password.txt`), the key that signs session cookies (at least 32 characters; unset, a random one is used and sessions end with the process), the session length (default 12 h), and whether cookies are marked `Secure` (set it once the UI is served over HTTPS). |
| `--auth-trusted-proxy`, `--auth-user-header`, `--auth-groups-header`, `--auth-name-header`, `--auth-role-map`, `--auth-default-role` | `OKF_WEB_AUTH_TRUSTED_PROXY`, `OKF_WEB_AUTH_USER_HEADER`, `OKF_WEB_AUTH_GROUPS_HEADER`, `OKF_WEB_AUTH_NAME_HEADER`, `OKF_WEB_AUTH_ROLE_MAP`, `OKF_WEB_AUTH_DEFAULT_ROLE` | `header` mode: the proxy's addresses or CIDR ranges (required; identity headers from any other peer are ignored; the word `any` believes every peer, for a server only the proxy can reach), the headers carrying the user (default `X-Forwarded-User`), the comma-separated groups (default `X-Forwarded-Groups`), and an optional display name, `group=role,...` (the highest matching role wins), and the role of a person in no mapped group (default `viewer`). |
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
- Read-only by default, and every write is a person's: the human workflow
  needs a writer connection *and* an identified person whose role allows the
  action; anonymous requests never reach the writer. Identities come through
  one seam (`auth.rs`): an OpenID Connect provider this site signs people in
  against, a trusted reverse proxy's headers, believed only from the proxy's
  own addresses, or a local users file (Argon2id hashes) with a login form.
  The two that end here issue the same HMAC-signed, `HttpOnly`,
  `SameSite=Lax` session cookie, which names the mode that opened it so one
  mode never honours another's, and carries no role: the role is derived on
  every request, so removing a user or changing the role map takes effect at
  once, and in `users` mode a changed password ends every session opened
  before it. Sign-ins are throttled per name (after five failures each
  attempt waits out a doubling cooldown), and an unknown name costs the
  same time as a wrong password. State-changing requests are refused when
  the browser says they came from another site (fetch metadata, else
  `Origin` against `Host`). In `header` mode the proxy MUST set the
  identity headers itself and never pass a client's copy through; the UI
  believes them only from the proxy's addresses, which is why
  `--auth-trusted-proxy` is required. Bind the UI to loopback (the default)
  and expose it through a reverse proxy that terminates TLS, or keep it on
  a private network.
- A verification is granted only by an approver: a `verified` list typed
  into an uploaded or edited document is set aside under
  `superseded_verifications` (with who, when, and why), never believed, so
  the trust tier cannot be claimed, and an edit sends a document back to
  review.
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

## Signing in with an identity provider (`oidc`)

The site is its own OAuth client, using the authorization code flow with
PKCE, which is what the OAuth 2.1 draft asks of a server-side client. It
works with any OpenID Connect provider: Entra ID, Okta, Keycloak, Auth0,
Google, a GitLab instance.

```sh
OKF_WEB_AUTH=oidc \
OKF_WEB_OIDC_ISSUER=https://id.example.com/realms/okf \
OKF_WEB_OIDC_CLIENT_ID=pgokf \
OKF_WEB_OIDC_CLIENT_SECRET=... \
OKF_WEB_OIDC_REDIRECT_URL=https://catalog.example.com/auth/callback \
OKF_WEB_AUTH_ROLE_MAP=okf-editors=editor,okf-approvers=approver,okf-admins=admin \
OKF_WEB_SESSION_SECRET=... \
pgokf-web
```

Register `https://<this site>/auth/callback` with the provider as the
redirect URI, and have it send a `groups` claim; the role map turns those
groups into roles, and a person in no mapped group gets
`--auth-default-role` (`viewer`).

What the sign-in insists on, in order: the provider's configuration is read
from `<issuer>/.well-known/openid-configuration` and must declare the
configured issuer; the request carries a fresh `state`, `nonce`, and PKCE
`S256` challenge, all kept in one short-lived signed cookie rather than in
memory, so a restart or a second instance loses nothing; the code is
exchanged over TLS directly with the provider, so it never passes through
the browser; and the ID token must be signed by a key the provider
publishes, with an asymmetric algorithm (`none` and the HMAC algorithms are
refused, since an HMAC one would let the provider's *public* key be used as
a shared secret), for the configured issuer, for this client, unexpired, and
with the `nonce` this site sent. Sign-out ends this site's session and,
where the provider advertises one, its own session too.

Two things to know. The person's identity becomes their OKF actor
(`human:<subject>`) and is recorded in every document they touch, so the
claim it comes from should be one the provider guarantees unique and never
reassigns; `sub` always is, while a user name or an email may be given to
someone else later, and the server says so at startup when the first
configured claim is not `sub`. And a role change at the provider takes
effect when the person signs in again (their mapped groups travel in the
session), while a change to the role map here takes effect at once; keep
`--session-hours` short if that matters.

## The human workflow

With a writer connection and an authentication mode, people with the right
role work on **content bundles** (bundles the catalog holds in its own
store, `register_bundle_content`, rebuilt from the sources the catalog
keeps, so `store_source` must be on) and, when `--bundles-dir` points at
the same directory the database reads, on **directory bundles** too: the
document is written in place and the bundle refreshed. A bundle synced
from an object store is changed at its source, and the UI says so.

Roles are a ladder; each holds the ones below it:

| Role | May |
| ---- | --- |
| `viewer` | read everything the reader role can see (everyone signed in; also everyone at all when identities are off) |
| `uploader` | **Upload** Markdown documents into a content bundle (new or existing); a document without `generated`/`author` is stamped with the person's OKF actor, `human:<name>` |
| `editor` | **Edit** a document (frontmatter and body, with a validating preview) or delete it; any earlier verification is set aside, `generated` names the editor, and the document returns to the review queue |
| `approver` | **Review**: the queue of unverified documents; approving records a `verified` event under the person's name (the document becomes *human-reviewed*, a draft becomes active), sending back makes it a draft and keeps the note under `reviews` |
| `admin` | everything above, plus **Admin**: people (in `users` mode: add, change role, reset password, remove; the file is rewritten in place and reloaded) and bundles (register a directory bundle, refresh, enable or disable, retire or bring back, unregister) |

Once identities are on (any mode but `none`), nobody reaches the site
without signing in: only the login page, the static assets, and the health
probe answer an anonymous request, so signing out ends access. Everyone
signed in has a **profile** page: their identity, what their role allows,
the documents they produced and verified, and (in `users` mode) a password
change.

Two limits worth knowing: the frontmatter is re-serialized when a document
is saved, approved, or sent back (key order is kept; YAML comments are
not), and bundle changes are serialized inside one UI process, so run one
instance of the workflow at a time.

Every decision is an ordinary OKF field in the document itself (`generated`,
`author`, `verified`, `status`, `reviews`, `superseded_verifications`), so
the trust tier and lifecycle status the catalog derives are the ones every
reader, agent, and plugin sees, and the extension's version history (when
`track_history` is on) keeps each revision.

## Running

```sh
OKF_PG_URL=postgresql://okf_reader:...@db:5432/okf \
OKF_WEB_BIND=127.0.0.1:8080 \
pgokf-web
```

With the human workflow on, behind a proxy that authenticates people:

```sh
OKF_PG_URL=postgresql://okf_reader:...@db:5432/okf \
OKF_PG_WRITER_URL=postgresql://okf_writer:...@db:5432/okf \
OKF_WEB_AUTH=header OKF_WEB_AUTH_TRUSTED_PROXY=10.0.0.5 \
OKF_WEB_AUTH_ROLE_MAP=okf-editors=editor,okf-approvers=approver \
pgokf-web
```

Or with a local users file:

```sh
printf '%s' 'a long password' | pgokf-web hash-password --user alice --role approver >> users
OKF_WEB_AUTH=users OKF_WEB_AUTH_USERS_FILE=users OKF_WEB_SESSION_SECRET=... pgokf-web
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
