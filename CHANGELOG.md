# Changelog

All notable changes to the `pgokf` PostgreSQL extension are documented in this
file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project aims to adhere to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The public API contract and what "a breaking change" means for this extension
are defined in [docs/api-stability.md](docs/api-stability.md).

## [Unreleased]

## [0.2.0] - 2026-09-08

**Skill packages are catalog content, and a web UI to work with them.** A
bundle may now carry [Agent Skills](https://agentskills.io/) packages whose
exact bytes the catalog stores and serves back; `pgokf-web` puts a UI and a
JSON API over the whole catalog, with an optional human workflow (upload,
edit, review) once an identity mode and a writer connection are configured;
`pgokf-workspace` turns a catalog selection into an agent-harness tree; and
`pgokf-mcp` speaks HTTP as well as stdio, with bearer tokens and roles.
`ALTER EXTENSION pgokf UPDATE TO '0.2.0'` is additive: it creates the new
tables empty. A bundle that already carried a `SKILL.md` projects into
`pgokf.skills` / `pgokf.scripts` / `pgokf.reference_documents` on its next
`refresh_bundle` (or re-registration) - including a resource-less manifest
whose bytes did not change, which the refresh now re-projects rather than
leaving a plain document.

### Added

- **Skill packages are catalog content, and plugins are built from them
  byte for byte** (specification §5.3, §15-§18, §21; the schema step to
  **0.2.0**). A bundle may carry [Agent Skills](https://agentskills.io/)
  packages: a directory with a `SKILL.md` and optional `scripts/`,
  `references/`, and `assets/`. Discovery (`okf-sync`) now classifies every
  file (`FileClass`: document, skill manifest, script, reference, asset,
  reserved) with the specification's precedence and the nearest-manifest
  ownership rule, and reads package resources whatever their extension.
  `okf-parser` gains `parse_skill_manifest`, which projects the portable
  Agent Skills frontmatter onto a virtual `type: Skill` concept (`title` =
  `name`, the `tags` extension, the complete frontmatter under
  `metadata.agent_skill`) and resolves the manifest's links to its own
  resources; structural findings against the standard are warnings. The
  extension stages scripts (UTF-8 required) and references/assets as
  virtual `Script` / `Reference` concepts whose ids are their full paths,
  keeps their **exact bytes** in three new tables (`pgokf.skills`,
  `pgokf.scripts`, `pgokf.reference_documents`) regardless of
  `store_source`, re-projects a package whenever any member changes (the
  §9 package hash), projects membership edges (`link_kind = 'package'`,
  `USES` / `REFERENCES`) and relabels the manifest's own links, and
  re-identifies a file whose class changes when a `SKILL.md` appears or
  disappears beside it. Three reader-level, tenant-scoped, audited readers
  return the bytes: `get_skill`, `get_script`, `get_reference` (composite
  types `skill_result`, `script_result`, `reference_result`; the access log
  records the three operations). `register_bundle_content` accepts a
  package in memory and refuses a loose non-Markdown path. Links resolve by
  target path as well as by id, so a document's link to a package's `.md`
  reference is an edge and survives the file becoming (or ceasing to be) a
  resource. Upgrade script `pgokf--0.1.16--0.2.0.sql`. Deferred to a later
  release: the `.okf-package.yaml` sidecar, standalone `type: Script` and
  typed `Reference` documents, the diagnostics table, `relationships`
  frontmatter, and visibility enforcement (the columns are
  projected and propagated but consulted nowhere - not in search, and not
  in retrieval, so `visibility` is descriptive in 0.2.0 and is not an
  access control).
- The plugin builder (`pgokf-workspace`, the web **Plugins** page, the MCP
  `build_workspace_plugin`) copies a selected skill package **whole**
  through the audited readers: `SKILL.md` unchanged, scripts executable,
  references and assets byte-identical, beside the knowledge skill for a
  native Agent Skills consumer (`.claude/skills/<name>/`) or under
  `knowledge/skills/<name>/` for the other shapes, with the package hash
  in the lockfile and a **Skills** section in every index; a resource
  selected on its own is written at its catalog path. The concept page
  gains a **Package** tab (resources with sizes and SHA-256s, exact-byte
  downloads through `/resource/...`), scripts and plain-text references
  render verbatim, binary assets say so, and the JSON API reports
  `package` / `resource`. New MCP tool `get_skill`.

- **`pgokf-web`, a web UI and JSON API companion.** Search (lexical on the
  configured backend, plus semantic and hybrid when an embeddings endpoint is
  configured) with facets and keyset paging, bundle browsing, concept pages
  with rendered Markdown, provenance, metadata, links with an interactive
  3D link graph (server-rendered SVG without WebGL), similar concepts and
  history, and an operations page over `health()`,
  `search_index_status()`, `get_config()`, stale and duplicate concepts, and
  the sync log. Filters without query text browse the matching concepts, so
  tag, type, status, and bundle links list directly. Read-only through
  `pgokf_reader` (page views read the projection tables; only the source
  download goes through the audited `get_concept_source()`); catalog content
  is sanitized before rendering and every response carries a
  Content-Security-Policy; statement, request, and pool-wait timeouts bound
  every request. Ships in the companions image and as the compose stack's
  `ui` profile (`PGOKF_UI_BIND`, `PGOKF_UI_PORT`, `PGOKF_UI_TITLE`,
  `OKF_UI_TENANT`).
- **`pgokf-workspace`, the workspace injector of spec §21 as a library**,
  and the two fronts over it: the web UI's **Plugins** page and the MCP
  server's `build_workspace_plugin` / `list_plugin_targets` tools. A
  catalog selection (bundle, query, types, tags, ids, trust, limit)
  becomes an Agent Skills package (`SKILL.md` plus one reference file per
  concept) for Claude Code, Codex, Hermes Agent, Kimi, Gemini CLI, Cursor,
  or the shared `.agents/skills/` directory, an `AGENTS.md` instruction
  file, an Ollama prompt bundle (Modelfile and system prompt), or a generic
  tree, always with `okf-workspace.yaml` and `okf-workspace.lock`. Optional
  components make it more than a skills directory: the harness's MCP
  server entry for `pgokf-mcp` (connection string referenced, never
  written), a catalog guide, and an `okf.sh` helper over the JSON API.
  Every target and MCP-configuration layout was read from the harness's
  documentation and is recorded with its source; unknown targets are
  refused. Builds are byte-identical for an unchanged catalog.
- **Graph explorer** (`/graph`): the catalog-wide link graph (best-connected
  concepts of the catalog or one bundle, coloured by bundle or type) or a
  concept's neighborhood, with clickable nodes and edges (hovering or
  selecting an edge highlights it and its ends; edge cards show the link
  texts; a node's card lists its connections for touch), zoom, center,
  2D/3D, and full-screen controls, a finder, and "explore from here".
  `/api/graph` serves the same document.
- **Portable Agent Plugins.** The builder's first target, `agent-plugin`,
  writes a self-contained directory as the Agent Plugins Specification
  1.0.0 defines it: `plugin.json` (the closed portable manifest, versioned
  by the catalog's newest sync plus a content digest), `skills/` holding the
  knowledge skill and every stored package byte for byte, and `mcp.json`
  with a typed stdio entry that starts `pgokf-mcp --env-file
  ${PLUGIN_DATA}/pgokf.env`, so no connection string and no ambient variable
  is needed. `pgokf-mcp` gained `--env-file`; a flag wins over the file and
  the file over the environment, so an installed plugin always talks to the
  catalog its own file names.
- **Picking specific files.** A selection may name files by identity
  (`picks`, `bundle_id:concept_id`) on top of the narrowing selectors; picks
  are ordered first so the limit never drops them, and a picked `SKILL.md`
  brings its whole package while a picked script or reference is one file.
  The builder page browses a bundle's files in a tree (directories, skill
  packages and their members marked) and ticks them into the selection; the
  MCP tool takes `picks`; `/api/bundles/{id}/tree` lists a bundle for it.
- The **Agent Plugin builder** page is a five-step flow: say what you are
  building (an Agent Plugin, a skills package, an instruction file, a
  prompt bundle, or generic files) and for which agent (a searchable list
  per kind; a name that is not listed is added as a new agent, with its
  skills directory for a skills package), name it, choose the content
  (browse a bundle or all bundles, find files, tick files or whole
  directories, or take everything in the scope and narrow it; every rule
  and ticked file is a removable chip in a selection bar), add extras (each
  extra's own fields appear only when it is ticked), then preview and
  download. The result summarizes the build in one line with the download
  button beside it; the MCP call is folded under it.
- **The human workflow and an authentication seam in `pgokf-web`.** With a
  writer connection (`OKF_PG_WRITER_URL`) and an authentication mode
  (`OKF_WEB_AUTH`: `header` for an authenticating reverse proxy whose
  identity headers are believed only from its own addresses, or `users` for
  people kept in the catalog (Argon2id hashes) with a login form and a
  signed session cookie), people work on content bundles by role: uploaders add Markdown
  documents (stamped with `generated`/`author` as `human:<name>` when
  absent), editors change or delete them with a validating preview (an edit
  sets aside earlier verifications and names the editor in `generated`),
  and approvers work a review queue, recording a `verified` event (the
  document becomes human-reviewed) or sending a draft back with a note.
  Every decision is an ordinary OKF field in the document, a `verified` list
  typed into an upload or edit is set aside rather than believed, roles are
  a ladder (viewer, uploader, editor, approver, admin), state-changing
  requests are refused across sites, and the catalog must keep sources
  (`store_source`). `pgokf-web user add` makes the first admin; the compose
  stack gained the matching `OKF_UI_*` settings.
- **`OKF_WEB_AUTH=oidc`: the UI as its own OAuth client.** A fourth
  implementation of the identity seam signs people in against any OpenID
  Connect provider (Entra ID, Okta, Keycloak, Auth0, Google, GitLab) with
  the authorization code flow and PKCE: the provider's configuration is
  read from its issuer and must declare it, `state`, `nonce`, and the PKCE
  verifier live in one short-lived signed cookie rather than in memory, the
  code is exchanged directly over TLS, and the ID token must be signed by a
  published key with an asymmetric algorithm (`none` and HMAC are refused),
  for the configured issuer and this client, unexpired, and carrying the
  nonce this site sent. Roles come from a groups claim through the same
  `OKF_WEB_AUTH_ROLE_MAP` the proxy mode uses, and sign-out ends the
  provider's session too where it offers one. The session cookie now names
  the mode that opened it, so a server reconfigured from one mode to
  another does not honour the old sessions.
- Once identities are on, nobody reaches the site without signing in
  (login page, static assets, and health probe excepted). Everyone signed in
  has a profile page (identity, what the role allows, documents produced and
  verified, a password change in `users` mode). The admin role has an admin
  page: people (add, change role, reset password, remove - each one
  statement against `pgokf_web.users`, in effect at once) and bundles (register a directory bundle,
  refresh, enable or disable, retire or bring back, unregister). With
  `OKF_WEB_BUNDLES_DIR` pointing at the directory the database reads
  (mounted read-write), editors change documents of directory bundles in
  place, written atomically and confined to the bundle, and the bundle is
  refreshed; the compose stack mounts the bundles into the UI for it.
- **`pgokf-mcp --http`: the MCP server over HTTP, with tokens and roles.**
  The same JSON-RPC messages the stdio transport carries, served at
  `POST /mcp` for clients that cannot launch a subprocess (a hosted agent, a
  fleet sharing one connection to the catalog). One implementation answers
  both transports, so they cannot drift apart; the only difference is who is
  asking. It is the Streamable HTTP transport without the parts a
  request/response server does not need: the server never speaks first, so
  it opens no event stream and issues no session id, and `GET`/`DELETE`
  answer `405`. `initialize` answers with the revision the client asked for
  (`2024-11-05`, `2025-03-26`, `2025-06-18`) and batches are refused.
  Over stdio the client already holds the connection string and there is
  nothing to authenticate; this endpoint is reachable, so it is never open.
  Every request carries a bearer token minted on the web UI's Admin page
  (or with `pgokf-web mcp-token mint`) and kept in the catalog as a SHA-256
  digest (`pgokf_web.mcp_tokens`, `pgokf_writer` only): the server, a
  reader, hashes the token a request presents - read only from the
  `Authorization` header, and only if it has the shape a minted token has -
  and asks `pgokf.mcp_token_bearer(digest)`, a `SECURITY DEFINER` lookup
  that answers for one digest and lists nothing, so the token never travels
  to the database and a reader learns nothing about tokens it does not hold,
  and a token minted for another tenant is refused. The check runs before
  the body is read and before a request takes a working slot, on a
  connection of its own so it never waits behind a tool call, bounded in
  number and in wait, so an anonymous caller can neither queue nor buffer
  anything and a flood of wrong tokens costs the catalog a fixed amount;
  revoking a token takes effect with the next request, since nothing is
  cached; the server proves the lookup exists before it opens a socket;
  refusals and outages are summarized into the log once a second rather
  than written per request; and `GET /healthz` answers 503 while either
  catalog connection, neither of which is re-established, is unwell. The token's
  role is the single decision point for what it may reach — `reader`
  searches and reads, `builder` may also build workspace plugins — and
  `tools/list` filters by the same answer `tools/call` enforces, showing
  neither a tool (`-32001` if called) nor an argument the caller may not
  use; a tool that does not exist is reported as unknown rather than as a
  refusal. A request carrying a browser `Origin` is refused unless the
  operator named it in `--allowed-origins`, which is the defence against a
  page in someone's browser reaching a server on their network, and a named
  origin gets the CORS answers a browser needs. Arguments that act on the
  server's own filesystem are marked in the tool schemas and refused over
  the network, so the next one is refused the day it is added. Bodies,
  their arrival, request concurrency, request time, and catalog statements
  are all bounded; TLS belongs in front of it, and the server says so if it
  binds an address reachable from elsewhere. New compose profile `mcp-http`
  with `PGOKF_MCP_BIND`, `PGOKF_MCP_PORT`, and `OKF_MCP_ALLOWED_ORIGINS`.
- **Built plugins can point at that endpoint.** The workspace injector's
  `mcp` component takes an `mcp_url` (the MCP tool's `build_workspace_plugin`
  argument, and a field on the web **Plugins** page beside the MCP command,
  which is its alternative): instead of configuring a server the harness
  starts, the entry describes the remote one it calls. Each harness's remote
  form was read from its own documentation and is recorded with its source,
  as every layout in the registry is: `type: http` with `url` and `headers`
  for Claude Code, Gemini CLI and Copilot, bare `url`/`headers` for Cursor,
  Kimi and Hermes, `url` with `bearer_token_env_var` for Codex, and
  `type: streamable-http` for a portable Agent Plugin. The bearer token is a
  secret, so it is treated exactly as the connection string is - referenced,
  never written: `${OKF_MCP_TOKEN}` where the harness expands one,
  `${env:OKF_MCP_TOKEN}` for Cursor, the variable *named* for Codex, and
  nothing at all for an Agent Plugin, whose specification forbids a
  credential in a package (and whose HTTPS-outside-loopback rule the build
  enforces). A harness that documents header values as literals - the
  Copilot CLI and Kimi - does not get an entry in its own configuration file
  at all: the build writes a fragment with a placeholder and the guide names
  the file to merge it into and says to keep that out of version control, so
  no file in the tree is ever meant to hold a secret. A URL carrying
  credentials, a query string, or a fragment is refused, `mcp_url` and
  `mcp_command` are alternatives, the manifest records the endpoint only
  when an entry was written for it, and the guide explains that a `reader`
  token is offered five tools and a `builder` seven. `list_plugin_targets`
  now reports each target's remote form, so an agent can see what `mcp_url`
  would do before calling it.
- Two layouts in the registry were **corrected** while reviewing that work,
  and they change the stdio builds too: Hermes Agent documents `${VAR}`
  references in any string value of a server entry, so its snippet now
  references `${OKF_PG_URL}` instead of carrying a placeholder connection
  string; and Kimi reads only `~/.kimi/mcp.json`, with no project-level
  file, so its entry is written as `okf-mcp.json` to merge rather than as
  `.kimi/mcp.json` the harness would never have read.
- **GitHub Copilot** is a target (`copilot`: `.github/skills/`, MCP entry
  in `.github/mcp.json` as a typed `local` server that inherits Copilot's
  environment). **Custom agents:** `target: custom` with a harness
  description (name, kind, skills directory) builds for an agent the
  registry does not know, laid out like the kind's base profile; the web
  builder adds one when a typed name is not in the list, and the MCP tool
  takes `harness`. Selections gained `all` (start from everything visible,
  so a rule narrowed only by types, tags, or a query needs no bundle).
- The UI is laid out for phones: scrolling navigation and tab strips,
  tables that scroll inside their panels, results before filters, the
  builder's target list folded behind its summary, and graph controls on
  the canvas.
- The OpenAI-compatible embeddings client moved from `pgokf-embed` into
  `pgokf-companion` (feature `embeddings`), shared with `pgokf-web`;
  `pgokf-pgconn` exposes `parse_config` and `rustls_connector` for pool
  builders.

### Changed

- **Package bytes are stored whatever `store_source` says.** The setting
  governs whether a *document's* source is kept; a package's `SKILL.md`,
  scripts, references and assets are the content, so they are kept
  regardless and `get_skill` / `get_script` / `get_reference` always return
  them.
- **A session cookie names the mode that issued it**, so a server
  reconfigured from one identity mode to another does not honour sessions
  opened under the old one. Everyone signs in again after such a change.
- **Everything under a package's `scripts/`, `references/` or `assets/`
  belongs to that package**, whatever it is called. A reserved name
  (`index.md`, `log.md`) there is an ordinary resource, and a `SKILL.md`
  there is a resource too rather than a second package - which used to take
  every file beside it out of the enclosing package, silently. A bundle
  with such a file gains members on its next refresh.
- **Two registry layouts were corrected**, which changes the trees built for
  them: Hermes Agent documents `${VAR}` references in a server entry, so its
  snippet references `${OKF_PG_URL}` instead of carrying a placeholder
  connection string; and Kimi reads only `~/.kimi/mcp.json`, so its entry is
  written as `okf-mcp.json` to merge rather than as a `.kimi/mcp.json` it
  would never have read.
- **New ceiling `pgokf.max_bundle_bytes`** (1 GiB, `SIGHUP`) bounds a
  bundle's *total* discovered size. The per-file and file-count ceilings
  multiply out to far more than one sync can hold, and a package resource is
  now kept whole whatever its type. A bundle over the total is refused where
  it previously registered; raise the setting or narrow the includes.
- The web UI is read-only until a writer connection **and** an identity
  mode are configured; with both, the human workflow and the admin page are
  on. It was read-only in every configuration before this release.

### Fixed

- Building a BM25 index quoted the `default_text_search_config` value with a
  backslash-escaped quote (`\'`), which PostgreSQL refuses when
  `backslash_quote = off`, and a trailing backslash could run a value on
  into the statement when `standard_conforming_strings = off`. The literal
  is now built the way the server's own `quote_literal()` builds it - the
  quote doubled, the backslash escaped - which is correct under every
  setting. (Affects the BM25 backend shipped since 0.1.15.)

### Security

- The `okf.sh` helper a plugin build generates put the web URL inside a
  `${VAR:-...}` default branch, which a shell expands - so a `$(...)` in
  that URL ran as a command on whoever ran the helper, and the URL is a
  free-text build argument reachable from the builder. The value is now a
  single-quoted assignment, and the validator refuses shell metacharacters.
- A bundle file was re-read by path after discovery with no size ceiling
  and no link check, so a file could grow past `max_file_bytes` or be
  replaced by a symbolic link between the scan and the read - and a package
  resource is stored verbatim and served back byte for byte. The read is
  judged through the descriptor it opens and is capped.
- Uploading into an existing bundle name through the "new bundle" path
  called a full-snapshot resync with only the files in hand, deleting
  everything else in that bundle. It is refused. Replacing an existing
  document now needs the editor role, as the ladder always said.
- A disabled bundle stayed readable by direct URL: the source read, and the
  bundle page's own concept listing, omitted the `enabled` flag that search
  and browsing applied.
- Sign-in ran unbounded Argon2id verifications and threw its throttle on the
  user name alone, so an unauthenticated flood could exhaust memory and CPU
  and a stranger could hold any named account shut. Verifications are
  bounded; the throttle keys on the client address too, refuses an
  over-long name unheard, and is capped in size. Behind a reverse proxy the
  client address is taken from a trusted `X-Forwarded-For` hop
  (`OKF_WEB_AUTH_TRUSTED_PROXY`), so one attacker no longer shares a key
  with everyone arriving through the proxy.
- The directory-bundle editor wrote its temporary file with a call that
  followed a symbolic link, so a link planted at that path could redirect an
  edit outside the bundle. The temporary is now created fresh, refusing any
  link, as the workspace tree writer already did.
- An OpenID Connect login mapped by the `email` claim accepted an unverified
  address, so an IdP account carrying someone else's email could assume their
  actor. An `email`-derived identity now requires `email_verified`.
- Signing out only cleared the cookie in that one browser: a session cookie
  was a signed value the server could verify but not forget, so a copy taken
  beforehand kept working until it expired, and nothing could end a session
  early - not even for a person disabled at the identity provider. Every
  issued session is now recorded in the catalog (`pgokf_web.sessions`), and
  a cookie the catalog does not list is refused. Signing out ends that
  session everywhere, the profile page offers **sign out everywhere**, the
  admin page lists who holds live sessions and can end anyone's in either
  mode, and a changed password or a removed person ends theirs. A session
  is a lever, not a detector: a copied cookie works until its session is
  ended or expires.
- **The web UI's people, sessions, MCP tokens, and identity provider live
  in the catalog.** Four extension-owned tables, `pgokf_web.users`,
  `pgokf_web.sessions`, `pgokf_web.mcp_tokens`, and `pgokf_web.oidc`, hold
  the `users` mode's people, every live session of the `users` and `oidc`
  modes, the digests of the bearer tokens `pgokf-mcp` accepts over HTTP, and
  the identity provider an admin set up; they are granted to `pgokf_writer`
  only (a reader never sees a hash, a session identifier, which tokens
  exist, or the provider's settings), transactional, shared by every UI
  instance, and carried by `pg_dump`. **An admin sets the identity provider
  up on the Admin page** in `users` mode - issuer, client id and secret,
  callback URL, claims, and the group-to-role map - and the sign-in page then
  offers "Sign in with …" beside the password form; saving reads the
  provider's discovery document first, so a wrong issuer is refused before it
  is stored, switching the provider off or removing it ends the sessions it
  opened, and every UI instance picks a change up on its next sign-in. The
  client secret is stored sealed (AES-256-GCM under a key derived from
  `OKF_WEB_SESSION_SECRET`), the table refuses anything but the sealed form,
  and without a session secret of its own the UI keeps no client secret (a
  public client with PKCE still works). The env-configured `oidc` mode
  remains for a site with no local people at all. The
  Admin page mints a token - shown once, on the page that minted it, with
  `Cache-Control: no-store` and never in a URL - and revokes one;
  `pgokf-web mcp-token mint|list|revoke` does the same from a shell for a
  stack without the UI. A token is minted for the tenant the UI serves, and
  an MCP endpoint admits only tokens minted for its own. The UI reaches all
  three through a pool of
  its own on the writer URL - never shared with the human workflow's long
  resyncs, with a short statement budget, and probed at startup - so the
  `users` and `oidc` modes require `OKF_PG_WRITER_URL`. A catalog that
  cannot answer an identity lookup is a 503, never a wrong password or a
  sign-out. `pgokf-web user add` / `user set-password` (the password read
  from standard input) make and rescue people; the Admin page does the rest.
- Security headers, the Content-Security-Policy included, now reach the
  responses the authentication and same-site layers produce themselves.
- A built tree is written in full or not at all, cannot hold two paths that
  are one file on a case-insensitive filesystem, and cannot carry a segment
  that Windows would normalize into `..`. The lockfile hashes every file of
  the tree, the generated ones included.

## [0.1.16] - 2026-09-05

**Deny-by-default tenancy, on demand.** The new durable policy key
`require_tenant` (default `false`) makes a session that has not set
`pgokf.tenant` see nothing and refuse to ingest, instead of the see-all
behavior every earlier release had. Nothing changes until an administrator
turns it on; `ALTER EXTENSION pgokf UPDATE TO '0.1.16'` adds one column and
rewrites the thirteen tenant-isolation policies in place.

### Added

- **`require_tenant` policy key** and the reader-level
  `pgokf.tenant_required()` function every row-level-security policy consults
  (as an uncorrelated sub-select: one evaluation per statement, never a
  per-row call). With the policy on, an unscoped session sees no rows through
  the policies and every reader built on them, the `SECURITY DEFINER` readers
  apply the same rule (`health()` counts, `list_sync_log`, `list_sync_changes`,
  `list_access_log`, and the ParadeDB `bm25_hits` path come back empty;
  `get_concept_source` raises the same not-found `22023` a foreign tenant
  gets), and the ingestion tier, the bundle-addressed writers and exports, and
  `purge_retired` refuse it with SQLSTATE `42501`. `set_config` / `reset_config` never need a
  tenant, so the policy can always be turned off again. `health()` gains
  `tenant_required`. The function is executable by any role with `USAGE` on
  schema `pgokf`, because the policies depend on it.
- **Companions:** `pgokf-ingest` accepts `--tenant` / `OKF_TENANT` like
  `pgokf-embed` and `pgokf-mcp`; all three apply it through one shared
  `pgokf-pgconn` helper. The compose stack passes `OKF_EMBED_TENANT` and
  `OKF_INGEST_TENANT` through (next to the existing `OKF_MCP_TENANT`).

### Changed

- **`schedule_refresh` pins the bundle's tenant into the pg_cron job command**
  (`set_config('pgokf.tenant', ...)` before `refresh_bundle`), so the cron
  worker's own, tenant-less session satisfies the tenant rules. Jobs
  scheduled by earlier releases run the bare call; re-schedule them
  (`schedule_refresh` updates a job in place) before turning `require_tenant`
  on, or pin the job role's tenant with `ALTER ROLE ... SET pgokf.tenant`.
- The write-side tenant rule (`enforce_bundle_tenant`) now also refuses an
  unscoped session when a tenant is required, with a distinct `42501` message
  naming the fix, rather than the unknown-bundle `22023` used for cross-tenant
  ids.

## [0.1.15] - 2026-09-05

**A PostgreSQL-licensed BM25 backend.** The `bm25` search backend now runs on
a selectable provider, and Tiger Data's `pg_textsearch` (PostgreSQL license,
PostgreSQL 17 and 18) joins ParadeDB's `pg_search`. The Docker image bundles
`pg_textsearch` on the 17 and 18 images and no longer bundles `pg_search` by
default. `ALTER EXTENSION pgokf UPDATE TO '0.1.15'` adds one policy column
with its default and touches no existing row.

### Added

- **`bm25_provider` policy key** (`auto` | `pg_textsearch` | `pg_search`,
  default `auto`, which prefers `pg_textsearch` when installed). Both
  providers name their index access method `bm25` and cannot coexist in one
  database, so resolution is unambiguous. `rebuild_search_index()` builds the
  resolved provider's index; `search_index_status()` reports `bm25.provider`
  and `bm25.provider_setting`; `health().bm25_ready` accepts either provider.
- **`pg_textsearch` backend.** One expression index over title, description,
  and body with the catalog's text-search configuration (baked into the
  index; rebuild after changing it). A page is served by the provider's
  index-ordered top-k scan (`ORDER BY <expr> <@> to_bm25query(...) LIMIT n`)
  with the structured filters, the keyset predicate, and row-level security
  applied as ordinary quals on that scan - inline with invoker rights, no
  privileged helper - then ordered `rank DESC, bundle_id, concept_id` in SQL.
  Rows tying the page's last rank are read past the page so keyset pages
  tile exactly, up to 256 tied rows per boundary (beyond that a `WARNING`
  and approximate paging). Query text is plain terms, as it already was on
  `pg_search` (neither provider interprets web-search operators). Covered by
  new in-database tests (the query shape is asserted to plan as a `bm25`
  index scan) that run wherever `pg_textsearch` is preloaded, including CI's
  PostgreSQL 17 and 18 legs.
- **Image:** `WITH_PG_TEXTSEARCH` (default `auto`: installed on 17 and 18 from
  the pinned, checksum-verified release package via the shared
  `fetch-pg-textsearch.sh`, with its PostgreSQL-license notice under
  `/usr/share/doc`; skipped elsewhere). CI builds the 17/18 images with
  `WITH_PG_TEXTSEARCH=1` and the smoke test requires a provider there, so a
  stale checksum table fails the build rather than publishing a provider-less
  image. `WITH_PG_SEARCH` now defaults to `0`; the ParadeDB build path remains
  available. `pgokf-restore` skips a `pg_textsearch` extension entry the
  target cannot replay, as it already did for `pg_search`. The compose stack
  preloads `pg_textsearch`.

### Changed

- `search_index_status().bm25.available` and `health().bm25_ready` now
  follow the provider `bm25_provider` resolves to (they were "`pg_search`
  installed"), so a pinned provider that is missing shows as unavailable
  even when the other one is present, and the fallback warning names what
  is installed instead.
- `concept_search`, `find_similar`, and `concept_search_hybrid` are now
  `PARALLEL RESTRICTED` (executed in the leader of a parallel plan, never in
  a worker): the BM25 providers declare their scoring parallel-unsafe, and
  these functions run it through SPI. Results and signatures are unchanged;
  the upgrade script relabels the installed functions.
- Documentation for search backends, configuration, packaging, and the
  compose deployment covers provider selection and the licensing difference.
- **Compose stack:** `shared_preload_libraries` now names `pg_textsearch`
  instead of `pg_search` (overridable through the new `PGOKF_PRELOAD`
  variable, e.g. `pgokf,pg_cron` for a 15/16/19 image), and the ParadeDB-only
  `paradedb.planner_warnings` setting is gone. An older compose file that still preloads `pg_search`
  will not start the 0.1.15 image (the library is no longer in it): update
  the preload line, or build the image with `WITH_PG_SEARCH=1
  WITH_PG_TEXTSEARCH=0` to keep ParadeDB. A database that has `pg_search`
  created must drop it (on the old image, `DROP EXTENSION pg_search CASCADE`)
  before `pg_textsearch` can be created; the step-by-step migration is in
  [docs/compose-deployment.md](docs/compose-deployment.md#upgrading-from-0114-paradedb-pg_search-to-0115-or-later-pg_textsearch).
- **Test harness:** the in-database tests that need a preloaded BM25 provider
  now run when `PGOKF_TEST_PRELOAD` names it (pgrx starts its own instance,
  so the operator's cluster configuration never reached them, and they had
  been skipping silently), and fail instead of skipping when the requested
  provider is unusable.

## [0.1.14] - 2026-09-04

**Complete logical backups, production container packaging, and native
arm64.** The one in-database change makes `pg_dump` actually capture the
catalog; everything else is packaging, deployment, companion, and CI work.
`ALTER EXTENSION pgokf UPDATE TO '0.1.14'` adds a trigger and the backup
registrations and touches no existing row; a catalog upgraded with it is
identical to a fresh 0.1.14 install.

### Fixed

- **BM25 search works for non-superuser readers.** With `search_backend =
  bm25`, any session that is not the table owner - every production reader -
  got `Unsupported query shape` from pg_search: row-level security wraps
  `pgokf.concepts` in a security-barrier subquery its custom scan cannot plan.
  The hit query now runs through `pgokf.bm25_hits`, a `SECURITY DEFINER`
  helper (pinned `search_path`, execute granted to `pgokf_reader` only) that
  applies the same explicit `pgokf.tenant` predicate the policies enforce, so
  `concept_search` and the hybrid search keep their invoker-rights contract
  and the result set is unchanged. Covered by a new in-database test that runs
  wherever pg_search is preloaded.
- **`pg_dump` now captures the catalog.** PostgreSQL skips the contents of
  extension-owned tables unless the extension registers them with
  `pg_extension_config_dump`; pgokf never did, so every archive carried only
  the `CREATE EXTENSION` statement and a restore came back empty - despite the
  operations guide describing dumps as complete. Every `pgokf.*` and
  `pgokf_private.*` table and sequence is now registered (discovered from the
  extension's dependency graph, so future tables are covered automatically),
  and a `BEFORE INSERT` trigger on the singleton `pgokf_private.config` row
  folds the restored policy row into the one `CREATE EXTENSION` seeds instead
  of failing on a duplicate key. Registered relations and the trigger are
  asserted by new in-database tests, and the image smoke test restores a real
  archive.

### Added

- **Server image with the optional extensions built in.** The Docker image
  now ships pgvector, pg_cron, and ParadeDB pg_search (each a `WITH_*` build
  argument; pg_search is fetched from its pinned upstream release and verified
  against a committed SHA256 table), a `HEALTHCHECK`, an OCI version label,
  and first-init hooks that create the extensions when preloaded, create
  least-privilege login roles from `PGOKF_{ADMIN,WRITER,READER}_PASSWORD`, and
  apply a JSON `PGOKF_POLICY` through `pgokf.set_config`. It also carries
  `pgokf-backup` (a verified `pg_dump` + roles-dump tool with retention) and
  `pgokf-restore` (a single-transaction restore that skips the archive entries
  an initialized target cannot replay: pg_search's unowned `paradedb` schema
  and, for a differently named database, pg_cron's objects).
- **Companions image** (`Dockerfile.companions`): `pgokf-ingest`,
  `pgokf-embed`, and `pgokf-mcp` in one non-root image.
- **Multi-architecture images.** CI builds and smoke-tests both images
  natively on amd64 and arm64 runners and merges them into one manifest per
  tag (`ghcr.io/logicocean/pgokf:<version>-pg<major>`,
  `ghcr.io/logicocean/pgokf-companions:<version>`), so they run on x86 and
  arm64 servers and on Apple Silicon. The `.deb` matrix gained arm64 legs.
- **Reference compose deployment** (`deploy/compose/`, documented in
  `docs/compose-deployment.md`): server, embedding daemon, cron-driven
  backups, and optional ingestion and MCP services, all configured from one
  `.env`.
- **`pgokf-embed --watch` / `--interval`**: the embedder runs as a daemon that
  embeds newly registered or refreshed concepts every interval, reconnecting
  per pass and surviving transient outages. The loop and its SIGINT/SIGTERM
  shutdown live in the new `pgokf-companion` crate, shared with `pgokf-ingest`
  (which therefore now also stops cleanly on SIGTERM, i.e. `docker stop`).
- Packaging lint in CI (hadolint, shellcheck, `docker build --check`, compose
  render), a `.dockerignore`, and reusable smoke scripts
  (`packaging/docker/smoke-test.sh`, `smoke-test-companions.sh`).

### Changed

- The image's first-init SQL creates `vector` whenever present and `pg_search`
  / `pg_cron` when preloaded, instead of only `pgokf`.
- Documentation: new compose deployment guide; packaging, operations, search,
  and deployment-topology guides updated for the bundled extensions, the
  backup tool, the embedder daemon, and multi-arch publishing.

## [0.1.13] - 2026-08-29

**Relicensed to AGPL-3.0 plus commercial, and the first public release.** This
release also fixes a set of audited defects in the shipped catalog and
companions. **The in-database SQL surface is
unchanged from 0.1.12** - no table, type, function signature, index, grant,
comment, role, or configuration key is added, dropped, renamed, or rewritten, so
`ALTER EXTENSION pgokf UPDATE TO '0.1.13'` is a documented no-op that yields a
catalog identical to a fresh 0.1.13, with every bundle, concept, embedding, link,
history version, and provenance record intact. Every fix is internal code, a new
input validation, companion behavior, or documentation; loading the 0.1.13
shared library is what activates the corrected code paths. `api_stability` and
the no-data-loss upgrade guarantees are preserved.

### Changed

- **Relicensed to dual AGPL-3.0 + commercial** (previously MIT). Every crate in
  the workspace - the extension, the `okf-parser` / `okf-sync` libraries, and
  the companion tools (`pgokf-ingest`, `pgokf-embed`, `pgokf-mcp`,
  `pgokf-pgconn`) - is now `AGPL-3.0-only`, and a commercial license is
  available for use the AGPL does not permit. See [`LICENSING.md`](LICENSING.md)
  and [`COMM-LICENSE.md`](COMM-LICENSE.md).

### Security

- **Denial-of-service in `concept_neighbors` closed (HIGH).** The recursive
  traversal previously enumerated *every simple path* from the seed
  (≈`O(N^hops)`), so a reader calling `concept_neighbors(seed, 5)` on a dense
  bundle could spin the backend on millions of walk rows for a tiny answer. The
  traversal is rewritten as a set-based, cycle-safe **breadth-first search** that
  records the first (minimum-hop) visit of each neighbor and never re-expands a
  visited node - `O(V + E)` work - returning **identical results** (distinct
  neighbors, shortest hop distance, cycle-safe, active-bundle-scoped) for normal
  graphs. A dense `K30` bundle that formerly timed out now answers in
  milliseconds.
- **Embedding poisoning rejected at write (HIGH).** `set_concept_embedding` now
  validates that every element of the supplied vector is finite, raising SQLSTATE
  `22023` (naming the offending index) for a `NaN`/`Infinity` element *before* the
  upsert. Storage is `real[]`, so such a value inserted silently but was rejected
  by pgvector at every query/index cast - one bad write could break semantic and
  hybrid search and `rebuild_embedding_index` catalog-wide until the row was found
  and fixed.
- **`purge_retired` data-loss race closed (HIGH).** `purge_retired` snapshotted
  the eligible bundles and then hard-deleted each without re-checking, so a
  concurrent `unretire_bundle` that committed in between could have its restored
  bundle (and its concept history) silently deleted. The per-bundle delete now
  re-evaluates the eligibility predicate (`retired_at IS NOT NULL AND retired_at <
  now() - older_than`) atomically under the bundle advisory lock, skipping - never
  deleting - a bundle that is no longer eligible.
- **Multi-tenancy trust model documented honestly.** `docs/security.md` and
  `docs/multi-tenancy.md` now state plainly that the `pgokf.tenant` GUC is a
  **scoping selector, not a hard security boundary** against a tenant who can run
  arbitrary SQL (any session can `SET`/`RESET` it, and a pinned `ALTER ROLE`
  default is overridable in-session), that the unset default fails open to
  *see-all*, and that a hard boundary requires a constrained access layer that
  pins the GUC or a per-tenant-role model.

### Fixed

- **`log.md` midnight-timestamp corruption (MED).** A space-separated
  `YYYY-MM-DD HH:MM[:SS]` leading timestamp in a reserved `log.md` entry parsed to
  **midnight** (only the first whitespace token was read). It now parses to the
  real instant, while an untimestamped line still yields a `NULL` `logged_at` and
  the entry text stays lossless.
- **`concept_neighbors` NULL-bundle disambiguation counted inactive bundles
  (LOW).** With no `bundle_id`, disambiguation now counts only **active** bundles
  (`enabled AND retired_at IS NULL`), so a disabled/retired duplicate of a concept
  id no longer raises a spurious `22023` that blocked the only active bundle.
- **Self-linked seed excluded from its own neighbor set (LOW).** A concept that
  links to itself is no longer returned as its own neighbor.
- **Deterministic tiebreak for semantic/hybrid search (LOW).**
  `concept_search_semantic` and `concept_search_hybrid` now break equal
  distance/fused-score ties on `(bundle_id, concept_id)` for a stable order.

### Companions

- **Optional TLS to PostgreSQL for all three companions (MED).** `pgokf-ingest`,
  `pgokf-embed`, and `pgokf-mcp` now share a small `pgokf-pgconn` helper crate and
  accept a `--tls` flag (env `OKF_PG_TLS`); TLS is also enabled by
  `sslmode=require` in the connection string. When enabled, the link uses `rustls`
  (reusing the stack already pulled by `object_store`/`reqwest`, so `cargo deny`
  stays green) and verifies the server certificate against the platform trust
  store. **`NoTls` remains the default** for a local socket / trusted network.
- **`pgokf-ingest` unit tests.** Added AAA unit tests for the companion's
  path-derivation (prefix strip) and change-detection (content-hash) helpers.

### Internal

- New `crates/pgokf-pgconn` workspace crate centralizes the companions'
  PostgreSQL connect step. Version bumped to `0.1.13` across the workspace,
  `pgokf.control`, and `META.json`; a documented no-op
  `pgokf--0.1.12--0.1.13.sql` upgrade script ships. Regression tests (Rust unit
  tests and in-database `#[pg_test]`s) cover every fix above.

## [0.1.12] - 2026-08-28

**Companion tooling**: three new out-of-process binaries that pair with the
already-shipped catalog surface. **The in-database extension is functionally
unchanged from 0.1.11** - no table, type, function, index, grant, comment, or
configuration key is added, dropped, renamed, or rewritten, so
`ALTER EXTENSION pgokf UPDATE TO '0.1.12'` is a documented no-op that yields a
catalog identical to a fresh 0.1.12 with every bundle, concept, embedding,
history version, and provenance record intact. All three tools do their network
and credential handling **outside** PostgreSQL and reach the catalog only
through its public SQL functions, preserving the extension's no-network-I/O
guarantee.

### Added

- **`pgokf-embed`** (new `crates/pgokf-embed`) - the reference **embedding
  generator** that pairs with the shipped semantic search. A standalone async
  binary that connects as a `pgokf_writer` role, finds concepts in
  `pgokf.concepts` with no matching `pgokf.concept_embedding` row (optionally
  scoped by `--bundle`), builds a bounded `title + description + body_text`
  text per concept, calls a configurable **OpenAI-compatible** `/v1/embeddings`
  endpoint (`POST {endpoint}/v1/embeddings` with `{model, input}` and a
  `Bearer` token) in batches, and streams each returned vector back through
  `pgokf.set_concept_embedding(bundle_id, concept_id, embedding)`. The endpoint,
  model, and API key are supplied by CLI/env and are **never** stored in
  PostgreSQL or hard-coded; the target dimension comes from `pgokf.get_config`
  (`embedding_dim`) or `--dim`. Any OpenAI-compatible server works - OpenAI, a
  local `text-embeddings-inference` / `llama.cpp` server, or a mock.
- **`pgokf-mcp`** (new `crates/pgokf-mcp`) - a **Model Context Protocol** server
  that exposes the catalog to AI agents over stdio JSON-RPC 2.0. A hand-rolled,
  dependency-light server (no MCP SDK) implementing the MCP handshake
  (`initialize` → `serverInfo`/`capabilities`, `tools/list`, `tools/call`) and
  four tools backed by the shipped functions: `concept_search`
  (`query`, `bundle_id?`, `limit?`, plus `type`/`tags`/`status`/`trust_tier`
  filters), `find_similar` (`concept_id`, `bundle_id?`, `limit?`),
  `concept_neighbors` (`concept_id`, `max_hops?`, `bundle_id?`), and
  `get_concept` (`concept_id`, `bundle_id?`). Connection string and optional
  `pgokf.tenant` come from CLI/env.
- **`pgokf-ingest --watch`** - the mountless ingestion companion gains a **watch
  daemon** mode (`--watch`, with `--interval` seconds, default 60) that
  periodically re-lists the object store and re-ingests through
  `register_bundle_content` (which diffs server-side, so a changed object
  resyncs on the next pass). A content hash of the collected object set lets an
  unchanged pass skip the round-trip. One-shot mode (no `--watch`) is unchanged.
  Shutdown is graceful on `SIGINT`.

### Changed

- The workspace version is `0.1.12`; the extension's `default_version`,
  `META.json`, and the runtime `pgokf.version()` all report `0.1.12`. The
  release ships the no-op `sql/pgokf--0.1.11--0.1.12.sql` upgrade script.

## [0.1.11] - 2026-08-28

**Opt-in concept version history**: an append-only SCD Type-2 version trail of
each concept, with point-in-time queries - *"what did this runbook say last
Tuesday?"*. The feature is **off by default**: with the new `track_history` key
disabled (the default), a sync records nothing and an existing install behaves
**exactly as before with zero extra storage**, which is what keeps the release
backward compatible. Everything is additive, so
`ALTER EXTENSION pgokf UPDATE TO '0.1.11'` migrates an existing install in a
single transaction and yields a catalog identical to a fresh 0.1.11. The two new
config columns are backfilled by their defaults (history off, retention 0).

### Added

- **`pgokf.concept_history` table** - an append-only SCD Type-2 version trail. One
  row per concept version with a per-concept monotonic `version` and a validity
  interval `[valid_from, valid_to)` (`valid_to IS NULL` = the current open
  version), a `change_kind` (`added` / `updated` / `removed`), and a snapshot of
  the concept core (`type`, `title`, `description`, `tags`, `resource`,
  `body_text`, `file_hash`) at that version. Populated only when `track_history`
  is on. Cascades from **`pgokf.bundles`** (not `pgokf.concepts`), so a removed
  concept keeps its history until the bundle is unregistered. Multi-tenant with
  the standard opt-in `tenant_id` row-level security and a
  `(bundle_id, concept_id, valid_from)` lookup index; `SELECT` granted to
  `pgokf_reader`.
- **`pgokf.concept_history(bundle_id, concept_id, max_rows DEFAULT 100)`** - the
  version timeline for one concept, newest first, as `SETOF pgokf.concept_version`.
  Reader-level, `STABLE`, invoker rights (the caller's tenant RLS applies).
- **`pgokf.concept_as_of(bundle_id, concept_id, as_of)`** - the single version
  valid at an instant (`valid_from <= as_of AND (valid_to IS NULL OR as_of <
  valid_to)`), or zero rows if the concept did not exist or had been removed then.
  The point-in-time answer. Reader-level, `STABLE`, invoker rights.
- **`pgokf.concept_version`** composite (`version`, `valid_from`, `valid_to`,
  `change_kind`, `type`, `title`, `description`, `file_hash`) - the row shape both
  readers return.
- **`track_history`** configuration key (`boolean`, default `false`) - the opt-in
  switch. When on, every register/refresh/content sync records history from its
  delta inside the same transaction, so history commits atomically with the sync:
  an added concept starts at version 1; an updated concept closes its open version
  and appends the next; a removed concept closes its open version and appends a
  zero-width removal tombstone. Documented as a storage/retention tradeoff.
- **`history_retention_days`** configuration key (`integer`, default `0` = keep
  indefinitely) - bounds history growth. When positive, closed versions
  (`valid_to IS NOT NULL`) older than the window are pruned in the same
  transaction after each sync; the single current open version of a concept is
  never pruned.

### Compatibility

- **Backward compatible and opt-in.** With `track_history` off (the default) no
  `pgokf.concept_history` row is ever written and there is zero behavior or
  storage change; the new reader functions simply return no rows. Enabling
  `track_history` is not retroactive - recording begins at the next sync, and a
  concept first versioned afterward begins its chain at that sync's `change_kind`.
- Version-history intervals are contiguous and non-overlapping per concept, with
  exactly one open version per live concept; each sync stamps its rows with a
  single captured instant so a closed version's `valid_to` abuts the next
  version's `valid_from`.
- The public function-surface count rises from 36 to 38 (`concept_history`,
  `concept_as_of`); see [docs/api-stability.md](docs/api-stability.md).

## [0.1.10] - 2026-08-28

**OKF-conformance batch**: an Attested Computation concept's type-specific
reference fields now become traversable graph edges, and the reserved per-
directory `log.md` activity log is now projected instead of dropped. Everything
is additive and backward compatible, so `ALTER EXTENSION pgokf UPDATE TO '0.1.10'`
migrates an existing install in a single transaction and yields a catalog
identical to a fresh 0.1.10. The one new `pgokf.links` column is backfilled by
its default.

### Added

- **Attested Computation reference fields as graph edges** - for a concept whose
  `type` is `Attested Computation`, its `computation`, `executor`, and `attester`
  reference fields (each a bare resource path or a `{resource: …}` mapping) are
  resolved into **`pgokf.links`** as typed internal edges, numbered after the
  concept's body links. `pgokf.concept_neighbors` now traverses them like any
  resolved internal edge, so the executor/attester/computation concepts are
  reachable even when the body links to none of them. A missing, external, or
  dangling reference is retained as `is_external` / `resolved = false` and never
  traversed, exactly like any other link. Non-attested concepts are unaffected.
- **`pgokf.links.link_relation`** (`text NOT NULL DEFAULT 'reference'`) - a new
  additive column carrying the edge's semantic relation, distinct from the
  Markdown-construct `link_kind`: `reference` for every ordinary link, or
  `attestation:computation` / `attestation:executor` / `attestation:attester`
  for the new typed edges. Existing rows are backfilled to `reference`.
- **Reserved `log.md` projection** - the per-directory OKF `log.md` activity
  logs, previously skipped entirely, are now parsed and projected into a new
  **`pgokf.bundle_log`** table (`bundle_id`, `tenant_id`, `directory`, `ordinal`,
  `logged_at`, `entry`; PK `(bundle_id, directory, ordinal)`; cascades from
  `pgokf.bundles`; opt-in multi-tenant RLS). Each non-blank line becomes one
  entry, with a leading ISO 8601 timestamp lifted into `logged_at` and the line
  stored losslessly. The projection is replaced wholesale on every sync, so it
  tracks edits/additions/removals; a `log.md` is still **never** a concept and
  never counts toward the bundle's `file_count`. `index.md` handling is
  unchanged.
- **`pgokf.list_bundle_log(bundle_id bigint, directory text DEFAULT NULL,
  max_rows int DEFAULT 500)`** (`SETOF pgokf.bundle_log_entry`, reader-level,
  `STABLE PARALLEL SAFE`, invoker rights) - lists a bundle's log entries ordered
  by directory then ordinal, optionally scoped to one directory (`''` for the
  root). New composite **`pgokf.bundle_log_entry(bundle_id, directory, ordinal,
  logged_at, entry)`**. Raises `22023` when `max_rows < 0`.

## [0.1.9] - 2026-08-28

**Search and scheduling batch**: keyset pagination and faceted counts on search,
a search-index coverage report, and an optional `pg_cron` scheduled re-sync.
Everything is additive and backward compatible, so
`ALTER EXTENSION pgokf UPDATE TO '0.1.9'` migrates an existing install in a single
transaction and yields a catalog identical to a fresh 0.1.9. `concept_search`
gains one optional trailing argument (documented below); no existing type or
default changes.

### Added

- **Keyset / cursor pagination on `concept_search`** - a new optional trailing
  argument **`after_cursor jsonb DEFAULT NULL`**. Ranked results now have a stable
  total order (`rank DESC`, then `bundle_id ASC`, then `concept_id ASC`); copy the
  `rank`, `bundle_id`, and `concept_id` of a page's last row into `after_cursor`
  and the next page continues strictly after it, with **no `OFFSET` drift and no
  duplicates or skips even when ranks tie**. Applied in both the native and BM25
  backends. A malformed cursor raises `22023`. The historical three- through
  seven-argument calls are unchanged (`after_cursor` defaults to the first page).
- **Faceted result counts** - **`pgokf.search_facets(query, bundle_id DEFAULT
  NULL, facet DEFAULT 'type', concept_type DEFAULT NULL, tags DEFAULT NULL, status
  DEFAULT NULL, trust_tier DEFAULT NULL)`** (`SETOF pgokf.search_facet`,
  reader-level) counts the same matching set `concept_search` would, grouped by
  one facet - `type`, `bundle`, `status`, `trust_tier`, or `tag` (any other value
  raises `22023`; the facet is dispatched on, never interpolated). The `tag` facet
  counts a concept once per tag. New composite **`pgokf.search_facet(facet_value
  text, count bigint)`**.
- **Search-index health / coverage** - **`pgokf.search_index_status()`**
  (`jsonb`, reader-level) reports the configured backend, that native FTS is
  always available, and for each optional index whether its extension is
  installed, whether the index exists, and how much of the catalog it covers
  (BM25 rows and embedding-vector coverage vs. total concepts). Coverage counts
  are tenant-scoped.
- **Optional `pg_cron` scheduled re-sync** - **`pgokf.schedule_refresh(bundle_id,
  schedule)`** (`text`, admin-tier) registers an idempotent
  `pgokf_refresh_<bundle_id>` cron job running `SELECT pgokf.refresh_bundle(<id>)`
  on the given schedule, and **`pgokf.unschedule_refresh(bundle_id)`** (`boolean`,
  admin-tier) removes it. The coupling to `pg_cron` is runtime-only (mirroring the
  pgvector / `pg_search` optional-dependency seam): `CREATE EXTENSION pgokf`
  succeeds without `pg_cron`, and when it is absent `schedule_refresh` raises a
  clear `22023` naming the missing dependency while `unschedule_refresh` is a
  clean no-op. Full scheduling requires `pg_cron` in `shared_preload_libraries`.

### Changed

- **`concept_search` result order is now a stable total order** (`rank DESC,
  bundle_id ASC, concept_id ASC`), replacing the previous `rank DESC, concept_id
  ASC`. This only refines the tiebreak for equal-rank hits and is what makes
  keyset pagination exact.
- The `0.1.8 → 0.1.9` upgrade replaces the seven-argument `concept_search`
  overload with the eight-argument superset (`DROP` old + `CREATE` new, in one
  transaction), exactly as `0.1.5 → 0.1.6` did, so an upgraded catalog carries a
  single `concept_search` overload identical to a fresh install.

## [0.1.8] - 2026-08-28

**Lifecycle and audit batch**: a per-sync change manifest, a reversible bundle
retirement window, an exfiltration/access audit, and cross-bundle content
deduplication. Everything is additive and backward compatible, so
`ALTER EXTENSION pgokf UPDATE TO '0.1.8'` migrates an existing install in a single
transaction and yields a catalog identical to a fresh 0.1.8. Six new public
functions are added; no existing signature, type, or default changes.

### Added

- **Per-concept change manifest** - every `register` / `refresh` / `content`
  sync now records which concepts it added, updated, or removed, not just the
  aggregate counts. Stored in the new administrator-only
  `pgokf_private.sync_log_change` (a child of `pgokf_private.sync_log`, cascading
  on delete so it shares the `sync_log_retention_days` window) and read through
  the reader-level **`pgokf.list_sync_changes(sync_id, max_rows DEFAULT 1000)`**
  (`SETOF pgokf.sync_change`), tenant-scoped like `list_sync_log`.
- **Bundle retirement / soft-delete window** - a new `bundles.retired_at`
  timestamp and three functions: **`pgokf.retire_bundle(bundle_id)`** and
  **`pgokf.unretire_bundle(bundle_id)`** (writer-tier), and
  **`pgokf.purge_retired(older_than interval DEFAULT '7 days')`** (admin-tier).
  A bundle is *active* only when `enabled AND retired_at IS NULL`; a retired
  bundle is excluded from `concept_search`, `concept_neighbors`, semantic/hybrid
  search, and the default `list_bundles` without deleting any rows, so retirement
  is a reversible undo window for the hard `unregister_bundle` cascade.
  `purge_retired` hard-deletes bundles retired longer than the interval (writing
  one `unregister` audit row each). Retirement is idempotent (re-retiring keeps
  the original instant) and does not touch `enabled`.
- **Exfiltration / access audit** - the three content-exporting operations
  (`export_parquet`, `export_sources`, `get_concept_source`) now each append one
  row to the new administrator-only `pgokf_private.access_log` (who read/exported
  what, and when), read through the admin-tier
  **`pgokf.list_access_log(bundle_id DEFAULT NULL, max_rows DEFAULT 100)`**
  (`SETOF pgokf.access_log_entry`). The log shares the `sync_log_retention_days`
  retention window.
- **Cross-bundle content deduplication** -
  **`pgokf.duplicate_concepts(bundle_id DEFAULT NULL, min_group int DEFAULT 2)`**
  (`SETOF pgokf.duplicate_group`, reader-level) groups byte-identical concepts by
  their stored BLAKE3 `file_hash`, so an operator can find the same runbook or
  reference copied across bundles.
- **`retired_at`** on the `pgokf.catalog_stat` composite (returned by
  `catalog_stats`), so retired bundles - hidden from `list_bundles` - stay
  visible with their retirement instant.

### Changed

- **`pgokf.get_concept_source`** is now `SECURITY DEFINER` and tenant-scoped (so
  it can append its access-audit row); its reader-tier grant and signature are
  unchanged.
- **`pgokf.list_bundles`** now excludes retired bundles by default (retired
  bundles remain reachable by id via `bundle_info` and visible in
  `catalog_stats`); disabled-but-not-retired bundles are still listed.
- The `sync_log_retention_days` policy now also governs `pgokf_private.access_log`
  and, transitively, the change manifest (via the `sync_log_change` cascade).

## [0.1.7] - 2026-08-28

**Opt-in multi-tenant isolation**, built from a per-session GUC and PostgreSQL
row-level security. Everything is strictly backward compatible: an existing
install, and any session that never sets a tenant, sees all rows and behaves
exactly as under 0.1.6, so `ALTER EXTENSION pgokf UPDATE TO '0.1.7'` migrates an
existing install in a single transaction and yields a catalog identical to a
fresh 0.1.7 (every existing row backfills to the `default` tenant). No public API
surface changes - no new functions, types, or arguments.

### Added

- **Denormalized `tenant_id`** (`text NOT NULL DEFAULT 'default'`) on every
  projection table - `bundles`, `concepts`, `concept_metadata`, `links`,
  `concept_provenance`, `concept_verification`, `concept_provenance_source`,
  `concept_source`, `concept_embedding` - and on `pgokf_private.sync_log`.
  Indexed where it helps (a dedicated index on `concepts`; on `bundles` the new
  `UNIQUE (tenant_id, path)` index already leads with it).
- **`pgokf.tenant` GUC** (`USERSET`, empty default) - the per-session tenant
  selector. Set it per session (`SET pgokf.tenant = 'acme'`), per login role
  (`ALTER ROLE r SET pgokf.tenant = ...`), or as a connection option; empty (the
  default) means the session declares no tenant and sees every row.
- **Row-level security on every projection table** with an opt-in-by-usage
  policy: a session that has not set `pgokf.tenant` matches all rows (backward
  compatible), a session that has set it matches only that tenant. RLS is enabled
  but *not forced*, so the `SECURITY DEFINER` write/admin functions bypass it to
  stamp and read within one single-tenant bundle.
- **`docs/multi-tenancy.md`** documenting the model, the per-tenant bundle keys,
  the `SECURITY DEFINER`-bypass reasoning, and the strict-isolation contract.

### Changed

- **Per-tenant bundle registration key.** `pgokf.bundles` is now keyed
  `UNIQUE (tenant_id, path)` instead of `UNIQUE (path)`, so two tenants may
  register the same filesystem or `content:<name>` path as independent bundles.
  The duplicate-registration `23505` check is scoped to the current tenant. (The
  upgrade replaces the old single-column key with this strict superset; no data
  is touched.)
- **Writes stamp the tenant.** `register_bundle` / `register_bundle_content`
  stamp the bundle row from `effective_tenant()`; every projected child row and
  the `set_concept_embedding` row inherit the bundle's tenant; the `sync_log`
  row records the operating tenant. `refresh_bundle`, `unregister_bundle`, and
  `set_bundle_enabled` operate on an existing bundle and never change its tenant.
- **`list_sync_log` and `health` are tenant-scoped.** Both are `SECURITY DEFINER`
  (they bypass RLS), so they apply the same opt-in tenant filter explicitly:
  `list_sync_log` filters its rows and `health`'s `bundle_count` / `concept_count`
  are scoped, each a no-op for an unset session.

## [0.1.6] - 2026-08-28

An additive **search-enhancement** batch: structured filters on ranked search, a
content more-like-this, and an optional pgvector semantic / hybrid surface.
Everything is backward compatible - the historical `concept_search(query,
bundle_id, limit_count)` call is unchanged - so `ALTER EXTENSION pgokf UPDATE TO
'0.1.6'` migrates an existing install in a single transaction and yields a
catalog byte-identical to a fresh `0.1.6` (verified by diffing the two).

### Added

- **Structured filters on `pgokf.concept_search`.** Four optional trailing
  arguments, each a no-op when `NULL`: `concept_type text`, `tags text[]`
  (**ALL-of** containment - a hit must carry every listed tag), `status text`,
  and `trust_tier text` (matched against `pgokf.concept_provenance`). The filters
  are parameter-bound `AND` clauses applied in both the native and BM25 backends,
  reusing the existing `tags`, `type`, and provenance indexes.
- **`pgokf.find_similar(concept_id text, bundle_id bigint DEFAULT NULL,
  limit_count int DEFAULT 10)`** - content more-like-this. It extracts a seed
  concept's most salient `body_tsv` lexemes and ranks other concepts against them
  through the configured `search_backend` (native FTS or BM25), excluding the
  seed. Distinct from `concept_neighbors` (the authored link graph).
- **Optional semantic + hybrid search via pgvector** (mirroring the optional
  BM25 seam exactly - `CREATE EXTENSION pgokf` still succeeds without pgvector):
  - **`pgokf.concept_embedding`** stores per-concept vectors as the builtin
    `real[]` (never a `vector` column, so the extension takes no static pgvector
    dependency), cast to `vector(embedding_dim)` only at query and index time.
  - **`pgokf.set_concept_embedding(bundle_id, concept_id, embedding real[])`**
    (writer-tier) is how a companion embedder streams caller-computed vectors in;
    the extension never computes embeddings or performs network I/O.
  - **`pgokf.concept_search_semantic(query_embedding real[], …)`** ranks by
    pgvector cosine distance; the `rank` column is the normalized cosine
    similarity. It **requires pgvector** and raises `22023` naming the missing
    dependency when it is absent (semantic search has no lexical fallback).
  - **`pgokf.concept_search_hybrid(query text, query_embedding real[], …)`** fuses
    the lexical and semantic results with **Reciprocal Rank Fusion** (RRF,
    k = 60) entirely in SQL. When pgvector is absent it degrades to lexical-only
    with a `WARNING`.
  - **`pgokf.rebuild_embedding_index()`** (admin-tier, mirroring
    `rebuild_search_index`) builds a pgvector HNSW cosine index for the configured
    dimension; a logged no-op when pgvector is absent or the dimension exceeds
    pgvector's 2000-dim HNSW limit.
  - New config key **`embedding_dim`** (integer, default 1536) governs the
    expected embedding length and the HNSW index typmod.

### Changed

- `pgokf.concept_search` gained the four trailing filter arguments (a new
  function *identity* in `pg_proc`). The upgrade script removes the superseded
  three-argument overload and creates the seven-argument one, so an upgraded
  catalog carries exactly one `concept_search` overload - identical to a fresh
  install - and every historical one-, two-, and three-argument call still
  resolves through the new defaults.

## [0.1.5] - 2026-08-28

An additive **audit, lifecycle, and observability** batch. Everything is
backward compatible - a new admin-only table, three composite types, five new
functions, three new configuration keys, and two functions whose *behavior*
gained a filter - so `ALTER EXTENSION pgokf UPDATE TO '0.1.5'` migrates an
existing install in a single transaction (the `0.1.4 → 0.1.5` upgrade script
adds the `sync_log` table, the three types, the five functions, and the two
config columns; the rest lives in the shared library and activates on load).

### Added

- **Sync/audit log.** A new administrator-only `pgokf_private.sync_log` records
  one row per successful `register` / `refresh` / `register_bundle_content` sync
  and per `unregister`, inside the operation's own transaction (so a logged row
  always means the operation committed). Read it with the reader-level
  **`pgokf.list_sync_log(bundle_id, max_rows)`** (returning the new
  `pgokf.sync_log_entry`). This also **activates the previously dead
  `sync_log_retention_days` key**: after each append, history older than the
  window is pruned in the same transaction (`0` keeps it indefinitely).
- **Bundle enable/disable lifecycle.** **`pgokf.set_bundle_enabled(bundle_id,
  enabled)`** (writer-tier) hides a bundle from ranked search *and* graph
  traversal without deleting any rows, and is fully reversible.
- **`concept_neighbors` now excludes disabled bundles**, matching
  `concept_search`, so a disabled bundle's concepts are neither returned nor
  traversed.
- **Change notification.** A new `notify_channel` configuration key: when set to
  a safe channel identifier, a successful sync emits
  `pg_notify(<channel>, {bundle_id, op, added, updated, removed, total})`.
  Off by default (empty) with zero overhead.
- **Observability functions** (all reader-level): **`pgokf.catalog_stats()`**
  (per-bundle indexed-concept / link / resolved-link counts, sync recency, and a
  24-hour staleness flag → `pgokf.catalog_stat`), **`pgokf.health()`** (a
  `jsonb` liveness/readiness document: `ok`, counts, `search_backend`,
  `bm25_ready`, `in_recovery`, `roles_ok`, `config_ok`), and
  **`pgokf.stale_concepts(bundle_id, as_of)`** (concepts past their OKF
  `stale_after` → `pgokf.stale_concept`).
- **OKF version conformance.** A new `okf_version_policy` key (`warn` | `reject`,
  default `warn`): a bundle declaring an OKF `okf_version` this build does not
  support (only `0.2` / `0.2.x`) is warned about and indexed under `warn`, or
  rejected with `22023` under `reject`. An absent `okf_version` is unaffected.
  The `okf-parser` crate gains a small, centralized `is_supported_okf_version`.

### Changed

- `sync_log_retention_days` moves from **defined-but-dead** to **active** (see
  above). `notify_channel` and `okf_version_policy` are new, active keys.
- **Internal:** a behavior-preserving complexity refactor of the parser,
  config-coercion, and SPI-row-reading hot paths - a shared `spi_read` tuple
  helper (DRY), per-key config coercion/defaults, and decomposed ISO-8601
  parsers - dropping the worst function's cyclomatic complexity from 39 to 18
  with no change to any behavior, signature, SQL surface, or test.

## [0.1.4] - 2026-08-28

Two additive capabilities landed together: a **`pgokf_writer` ingestion role
tier** paired with an **optional BM25 search backend**, and a **mountless
object-store ingestion path** (`register_bundle_content` plus the standalone
`pgokf-ingest` companion). Everything here is backward compatible - new
functions, a new role, a new projection column, and a new configuration key -
so `ALTER EXTENSION pgokf UPDATE TO '0.1.4'` migrates an existing install in a
single transaction (the `0.1.3 → 0.1.4` upgrade script creates the writer role,
adds `rebuild_search_index`, and adds `register_bundle_content` +
`bundles.source_type`).

### Added

- **`pgokf_writer` role - a new ingestion tier** between `pgokf_reader` and
  `pgokf_admin` (`pgokf_reader` < `pgokf_writer` < `pgokf_admin`, each inheriting
  the tier below). It is the intended account for an automated ingestion
  pipeline: it can register/refresh/unregister bundles but cannot change
  configuration, write exports, or read `pgokf_private`.
- **`pgokf.register_bundle_content(name text, paths text[], contents bytea[], options jsonb)`**
  - the *mountless* ingestion path. A companion process reads an object store and
  streams the collected `(path, bytes)` into PostgreSQL; the extension itself
  performs no network or filesystem I/O. Re-calling it resyncs the bundle
  (changed concepts upserted, missing ones deleted) exactly like a filesystem
  refresh, with the same `max_bundle_files` / `max_file_bytes` bounds and
  `store_source` round-trip. Writer-tier, `SECURITY DEFINER`.
- **`pgokf.bundles.source_type`** (`'filesystem'` | `'content'`, default
  `'filesystem'`) distinguishing a bundle registered from a canonical on-disk
  root from one streamed in memory (keyed on the synthetic path `content:<name>`).
- **`pgokf.rebuild_search_index()`** - admin function that (re)builds the optional
  `pg_search` BM25 index; a no-op with a notice when `pg_search` is not installed.
- **`search_backend` configuration key** (`native` | `bm25`, default `native`).
  `native` uses the built-in `websearch_to_tsquery` / `ts_rank_cd` ranking;
  `bm25` routes `concept_search` through ParadeDB `pg_search` at runtime via SPI
  when available, falling back to native (with a warning) when it is not - so the
  extension takes no hard dependency on `pg_search`.
- **`pgokf-ingest` companion crate** - a standalone async binary that lists an
  S3-compatible object store (MinIO / SeaweedFS / AWS S3 / GCS / Azure via
  `object_store`), downloads the objects, and streams them to
  `register_bundle_content` as `pgokf_writer`. Object-store credentials live in
  the companion and never reach PostgreSQL. It is a separate workspace member and
  does not affect the extension build.

### Changed

- **Ingestion moved to the writer tier (backward compatible).**
  `pgokf.register_bundle`, `pgokf.refresh_bundle`, and `pgokf.unregister_bundle`
  now require `pgokf_writer` instead of `pgokf_admin`. Existing admin callers keep
  working because `pgokf_admin` inherits `pgokf_writer`; configuration and the
  file-writing exports remain admin-only.
- **`refresh_bundle` rejects content-sourced bundles.** A `source_type = 'content'`
  bundle has no filesystem root, so `refresh_bundle` raises `22023` for it -
  re-sync those by calling `register_bundle_content` again.
- **Internal:** the sync engine was refactored around a `ByteSource` seam so the
  filesystem path (walk + read) and the content path (caller-supplied bytes)
  share one classify → parse → upsert → project pipeline. Filesystem
  `register_bundle` / `refresh_bundle` behavior is unchanged.

## [0.1.3] - 2026-08-28

OKF v0.2 conformance re-model of the provenance / trust / lifecycle projection,
and population of `pgokf.bundles.okf_version`. This is a **breaking change** to
the `pgokf.concept_provenance` shape; because the extension is pre-release (no
tagged release, no external installs), the schema is changed in place with no
compatibility shim.

### Changed

- **`pgokf.concept_provenance` re-modeled to OKF v0.2 (breaking).** The invented,
  non-OKF columns `verified` (a flattened bool), `verification_method`, and
  `freshness` are removed. The table now carries the real OKF v0.2 fields:
  `generated_by` (`generated.by`), `generated_at` (`generated.at`), `status`
  (LIFECYCLE `status`), `stale_after`, `usage_window_from` / `usage_window_to`
  (top-level `usage_window`), and a `trust_tier` **derived** from the
  verification actors (`unverified` → `machine-confirmed` → `human-reviewed`).
  Timestamps are ISO 8601, parsed defensively (a malformed instant projects
  `NULL`, never aborting the sync); the recognized key subset is kept losslessly
  in `details`. The index on `verified` is replaced by an index on `trust_tier`.
- **`pgokf.bundles.okf_version` is now populated.** The sync engine reads the
  optional `okf_version` from the reserved bundle-root `index.md` frontmatter
  (string or number, e.g. `0.2`) and stores it; an absent or malformed value
  leaves the column `NULL`. It surfaces unchanged through `bundle_info` /
  `list_bundles`.

### Added

- **`pgokf.concept_verification` table** - the ordered OKF `verified[]` event
  list, one `(bundle_id, concept_id, ordinal)` row per `{by, at}` event (a single
  `verified` mapping is stored as one `ordinal = 0` row; actorless events are
  skipped). Cascades from `pgokf.concepts`; reader-`SELECT`able.
- **`pgokf.concept_provenance_source` table** - the OKF `sources[]` provenance
  materials, one row per entry (`source_id`, `resource`, `title`, `author`,
  `usage_count`, `last_modified`, per-source `usage_window_from` / `_to`).
  Distinct from the raw-bytes `pgokf.concept_source`. Cascades from
  `pgokf.concepts`; reader-`SELECT`able.

### Fixed

- **`export_parquet` epoch cast for OKF v0.2 provenance timestamps.** The
  re-modeled `pgokf.concept_provenance.generated_at` is a `timestamptz`;
  `export_parquet` now converts it to epoch microseconds
  (`(EXTRACT(EPOCH FROM generated_at) * 1000000)::bigint`) so the Parquet writer
  emits a portable `Timestamp(µs, UTC)` column. Verified round-trippable in
  DuckDB via an in-database test.

### Security

- **Closed an `export_sources` write-escape via a symlinked parent directory.**
  `export_sources` recreates a bundle's directory tree under `dest_dir`; a
  symlink planted at an intermediate path component could previously redirect a
  write outside the validated destination. Writes now use the same `O_NOFOLLOW`
  open as `export_parquet` on the final component and re-validate every stored
  concept path as a plain bundle-relative path, so a planted symlink is refused
  (`22023`) instead of followed. Each reconstructed file is additionally
  verified against its recorded BLAKE3 `file_hash` before creation
  (`XX000` on mismatch, nothing written).

### Upgrade

- No supported in-place upgrade from `0.1.2`: this pre-release drops and
  re-creates the provenance projection. Re-`CREATE EXTENSION` and re-register
  bundles; because the on-disk bundle is the source of truth, the projection is
  fully rebuilt from a sync.

## [0.1.2] - 2026-08-27

Additive, opt-in raw source storage. Default behavior is unchanged: the new
`store_source` policy is **off by default**, so an install that never enables it
is byte-for-byte identical to 0.1.1.

### Added

- **`store_source` configuration key** (boolean, default `false`) on
  `pgokf_private.config`. It selects between two deployment tiers: `true` stores
  each concept's verbatim source bytes in PostgreSQL (small, self-contained
  install - no external storage needed); `false` keeps the source in a mounted
  object store / data lake and PostgreSQL holds only metadata and search. Like
  `default_text_search_config`, it is read at sync time and is **not
  retroactive** - set it before the first `register_bundle`, or re-register.
- **`pgokf.concept_source` table** - opt-in verbatim source bytes
  (`raw_content bytea`, `byte_size integer`), keyed `(bundle_id, concept_id)` and
  cascading from `pgokf.concepts`, so removals and unregistration drop the stored
  source automatically. TOAST-compressed with `lz4` where the build supports it,
  otherwise `pglz`. Reader-`SELECT`able.
- **`pgokf.get_concept_source(bundle_id, concept_id) → bytea`** - reader-level
  retrieval of a concept's exact stored bytes to the client (no filesystem
  write). Raises `22023` when the concept exists but no source was stored, and,
  distinctly, when no such concept exists.
- **`pgokf.export_sources(bundle_id, dest_dir) → pgokf.export_result`** -
  admin-only reconstruction of a bundle's stored source files on disk,
  byte-for-byte. Reuses `export_parquet`'s destination validation and
  `O_NOFOLLOW` file creation, recreates the bundle-relative directory tree, and
  verifies each written file against the concept's BLAKE3 `file_hash`.

### Changed

- The sync engine now persists source bytes into `pgokf.concept_source` when
  `store_source` is enabled, projected inside the same atomic, advisory-locked
  transaction as links and provenance (no change when the key is off).

### Upgrade

- `ALTER EXTENSION pgokf UPDATE TO '0.1.2'` brings a 0.1.1 install fully to 0.1.2
  with no data loss: it adds the `store_source` column (default `false`), the
  `concept_source` table, and the two new functions, and touches no existing
  object.

## [0.1.1] - 2026-08-27

Hardening, performance, and packaging. No public-API change: the stable surface
(functions, types, tables, roles, GUCs) is byte-for-byte identical to 0.1.0, so
`ALTER EXTENSION pgokf UPDATE TO '0.1.1'` is a proven no-data-loss step.

### Fixed

- **A large concept body could abort an otherwise-valid sync.** The body
  `tsvector` is now fully bounded so no document within the configured size
  limits can raise PostgreSQL's `tsvector` size error mid-sync; the whole
  transaction no longer rolls back on a single large-but-in-limit file.
- **Resolved the findings from a full-repository adversarial audit** across the
  parser, sync engine, and catalog surface - input-validation edges, error
  mapping, and path-handling corners hardened without changing behavior for
  well-formed input.

### Performance

- **Batched SPI inserts in the sync engine** - concepts, metadata, links, and
  provenance are projected in batched statements instead of row-at-a-time,
  cutting per-file round trips on large bundles.
- **Guarded the link re-resolution `UPDATE`** so an incremental
  `refresh_bundle` only re-resolves links whose target set actually changed,
  avoiding needless writes on unchanged concepts.

### Added

- **Distribution packaging** - `.deb` / `.rpm` build recipes, a PGXN
  `META.json`, a Docker image, and a Homebrew formula, wired into a `packages`
  CI job so per-major artifacts build reproducibly.
- **Proven extension upgrade path.** The example `sql/pgokf--0.1.0--0.1.1.sql`
  upgrade script exercises `ALTER EXTENSION pgokf UPDATE TO '0.1.1'` end to end
  as a deliberate no-op, demonstrating the forward-compatible,
  never-`DROP`/`TRUNCATE`/`DELETE` migration contract that
  `tests/api_stability.rs` enforces on every shipped script.

## [0.1.0] - 2026-08-27

The first tagged release: a complete, transactional PostgreSQL catalog for
Open Knowledge Format (OKF) bundles. The bundle on disk stays the portable
source of truth; PostgreSQL becomes a projection optimized for metadata
queries, native full-text search, and link-graph traversal.

### Added

- **Bundle registration and sync.** `pgokf.register_bundle(path, name, options)`
  ingests an OKF bundle root and `pgokf.refresh_bundle(bundle_id)` incrementally
  re-synchronizes it, re-parsing only files whose BLAKE3 content hash changed
  and removing rows for deleted files. `pgokf.unregister_bundle(bundle_id)`
  removes a bundle; concepts, metadata, links, and provenance cascade.
- **Catalog projection.** Base tables `pgokf.bundles`, `pgokf.concepts`, and
  `pgokf.concept_metadata`, plus the feature projections `pgokf.links`
  (concept-to-concept link graph) and `pgokf.concept_provenance` (generation
  and verification lineage).
- **Full-text search.** `pgokf.concept_search(query, bundle_id, limit)` returns
  ranked hits with `ts_headline` snippets over a weighted `tsvector` (title A,
  tags/type/description B, body D). Native PostgreSQL FTS only - no third-party
  search extension is required.
- **Link-graph traversal.** `pgokf.concept_neighbors(concept_id, max_hops,
  bundle_id)` walks the resolved link graph outward from a concept.
- **Administration.** `pgokf.list_bundles()` and `pgokf.bundle_info(bundle_id)`
  expose the registered-bundle inventory as the `pgokf.bundle_info` type.
- **Durable configuration.** `pgokf.set_config`, `pgokf.reset_config`, and
  `pgokf.get_config` manage a single, typed, cluster-persistent policy row
  (`allowed_roots`, `default_text_search_config`, `default_strict`,
  `sync_log_retention_days`, `default_exclude`) stored in the
  administrator-only `pgokf_private.config` table.
- **Parquet export.** `pgokf.export_parquet(bundle_id, dest_dir)` writes a
  bundle's catalog projection to four Parquet files - `concepts.parquet`,
  `concept_metadata.parquet`, `links.parquet`, and `concept_provenance.parquet`
  - inside `dest_dir` for downstream analytics.
- **Version introspection.** `pgokf.version()` reports the loaded shared
  library's version for post-upgrade agreement checks.
- **Composite result types.** `pgokf.bundle_sync_result`,
  `pgokf.concept_search_result`, `pgokf.concept_neighbor`, `pgokf.bundle_info`,
  and `pgokf.export_result`.
- **Security model.** Two cluster roles, `pgokf_reader` (search and read
  configuration) and `pgokf_admin` (register/refresh/unregister and manage
  configuration, inherits `pgokf_reader`). Every mutating function is
  `SECURITY DEFINER` with a pinned `search_path`, `EXECUTE` is revoked from
  `PUBLIC` and granted only to the appropriate role, and bundle paths are
  validated (absolute, traversal-free, canonicalized, optionally confined to
  configured `allowed_roots`) before the server reads any file. The private
  `pgokf_private` schema is internal state, not API.
- **Configurable safety limits (GUCs).** `pgokf.max_file_bytes`,
  `pgokf.max_bundle_files`, `pgokf.max_frontmatter_bytes`,
  `pgokf.max_graph_hops`, and `pgokf.log_level`.
- **Documentation coverage.** Every public object - all 12 functions, all 5
  composite types, all 6 catalog tables, and both API roles - carries a
  `COMMENT ON`, enforced by the `api_stability` test suite and by a runtime
  `obj_description` coverage gate in the release checklist.
- **PostgreSQL 15–19 support**, built with Rust (edition 2024) and pgrx 0.19.
- **Extension upgrade mechanism.** A documented, forward-compatible example
  upgrade path (`pgokf--0.1.0--0.1.1.sql`) exercises
  `ALTER EXTENSION pgokf UPDATE` with a proven no-data-loss guarantee.

### Security

- Path traversal, symlink escape, NUL-byte, and relative-path inputs to
  `register_bundle` are rejected before any filesystem access.
- The `pgokf_private` schema and its `config` table are readable and writable
  only by the extension owner and `pgokf_admin`; readers cannot see policy.

[Unreleased]: https://github.com/LogicOcean/pgokf/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/LogicOcean/pgokf/compare/v0.1.16...v0.2.0
[0.1.16]: https://github.com/LogicOcean/pgokf/compare/v0.1.15...v0.1.16
[0.1.15]: https://github.com/LogicOcean/pgokf/compare/v0.1.14...v0.1.15
[0.1.14]: https://github.com/LogicOcean/pgokf/compare/v0.1.13...v0.1.14
[0.1.13]: https://github.com/LogicOcean/pgokf/compare/v0.1.12...v0.1.13
[0.1.12]: https://github.com/LogicOcean/pgokf/compare/v0.1.11...v0.1.12
[0.1.11]: https://github.com/LogicOcean/pgokf/compare/v0.1.10...v0.1.11
[0.1.10]: https://github.com/LogicOcean/pgokf/compare/v0.1.9...v0.1.10
[0.1.9]: https://github.com/LogicOcean/pgokf/compare/v0.1.8...v0.1.9
[0.1.8]: https://github.com/LogicOcean/pgokf/compare/v0.1.7...v0.1.8
[0.1.7]: https://github.com/LogicOcean/pgokf/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/LogicOcean/pgokf/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/LogicOcean/pgokf/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/LogicOcean/pgokf/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/LogicOcean/pgokf/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/LogicOcean/pgokf/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/LogicOcean/pgokf/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/LogicOcean/pgokf/releases/tag/v0.1.0
