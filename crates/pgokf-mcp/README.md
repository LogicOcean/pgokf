# pgokf-mcp

A **Model Context Protocol** server that exposes the [`pgokf`](../extension)
catalog to AI agents as MCP tools.

`pgokf-mcp` is a standalone async binary that speaks MCP over **stdio** by
default: newline-delimited JSON-RPC 2.0 on stdin/stdout. It implements the MCP
handshake (`initialize` → `serverInfo`/`capabilities`, then `tools/list` and
`tools/call`) and backs each tool with a query against the shipped `pgokf`
public functions. The JSON-RPC layer is **hand-rolled on `serde_json`** - no MCP
SDK dependency, so it adds nothing new to the workspace's `cargo deny` surface.

`--http <addr>` serves the same messages over HTTP instead, for clients that
cannot launch a subprocess. That endpoint is reachable, so it is never open:
every request carries a bearer token and the token's role decides which tools it
may call. See [Serving it over HTTP](#serving-it-over-http).

## Tools

| Tool | Arguments | Backed by |
| --- | --- | --- |
| `concept_search` | `query` (required), `bundle_id?`, `limit?`, `type?`, `tags?`, `status?`, `trust_tier?` | `pgokf.concept_search` |
| `find_similar` | `concept_id` (required), `bundle_id?`, `limit?` | `pgokf.find_similar` |
| `concept_neighbors` | `concept_id` (required), `max_hops?`, `bundle_id?` | `pgokf.concept_neighbors` |
| `get_concept` | `concept_id` (required), `bundle_id?` | `pgokf.concepts` projection |
| `get_skill` | `bundle_id` (required), `concept_id` (required) | `pgokf.get_skill`: an Agent Skills package stored in the catalog (metadata, the exact `SKILL.md` text, and the scripts, references, and assets it owns); audited |
| `list_plugin_targets` | none | the `pgokf-workspace` target registry: each harness's kind and documented directory |
| `build_workspace_plugin` | `target` (required; `agent-plugin` for a portable Agent Plugins 1.0.0 directory, a harness id, or `custom` with `harness` `{label, kind?, skills_dir?}` for an agent the registry does not know), `name?`, `title?`, `all?` (start from every visible concept, so a selection narrowed only by types, tags, or a query needs no bundle), `bundle_ids?`, `concept_ids?`, `picks?` (`"bundle_id:concept_id"` strings: specific files added to whatever else matches), `tags?`, `types?`, `query?`, `verified_only?`, `limit?`, `base_model?`, `components?` (`mcp`, `guide`, `tools`; default all), `mcp_command?` or `mcp_url?` (point the harness at a `pgokf-mcp --http` endpoint instead of a local server; the bearer token is referenced, never written), `web_url?`, `output_dir?`, `overwrite?` | `pgokf-workspace`: an Agent Skills package (`SKILL.md` + `references/`), an `AGENTS.md` instruction file, an Ollama prompt bundle, or a generic tree, with `okf-workspace.yaml` and `okf-workspace.lock`; skill packages stored in the catalog (`type: Skill`, select them with `types: ["Skill"]` or by id) are copied whole and byte for byte, scripts executable; with `output_dir` the tree is written into the workspace, otherwise the files are returned inline (up to 1 MiB; UTF-8 files as text, binary files as `{"encoding": "base64", "data": ...}`) |

Each tool returns an MCP tool result whose single text content block holds the
JSON the query produced: an array of rows for the search, graph, and concept
tools, one object for `get_skill` and `build_workspace_plugin`. A missing or
hidden skill surfaces as the catalog's own `22023` error text.

## Configuration

`--env-file <path>` reads `KEY=VALUE` lines (comments, quotes, and `export`
prefixes allowed) for `OKF_PG_URL`, `OKF_TENANT`, and `OKF_PG_TLS`. A flag on
the command line wins over the file, and the file wins over the process
environment, so an installed plugin always talks to the catalog its own file
names. An Agent Plugins package built by `pgokf-workspace` starts the server
with `--env-file ${PLUGIN_DATA}/pgokf.env`: the connection string lives in a
file you create once under the client's plugin data directory, never in the
package.

| Flag | Env | Meaning |
| --- | --- | --- |
| `--database-url` | `OKF_PG_URL` | PostgreSQL URL for a `pgokf_reader`-capable role (required) |
| `--writer-url` | `OKF_PG_WRITER_URL` | PostgreSQL URL for a `pgokf_writer`-capable role, which the `writer` and `admin` token roles need. Without it this server only reads, and their tools say so |
| `--tenant` | `OKF_TENANT` | Apply a `pgokf.tenant` scope for the session (multi-tenant isolation; required once the catalog's `require_tenant` policy is on) |
| `--tls` | `OKF_PG_TLS` | Require a TLS-encrypted link to PostgreSQL (default off) |
| `--http` | `OKF_MCP_HTTP_BIND` | Serve MCP over HTTP on this address instead of over stdio; every request then carries a bearer token minted on the catalog's Admin page |
| `--allowed-origins` | `OKF_MCP_ALLOWED_ORIGINS` | Browser origins allowed to call the HTTP endpoint, comma-separated (default: none, which refuses every request carrying an `Origin`) |

### PostgreSQL transport (TLS)

The database link is plaintext (`NoTls`) by default - fine for a local socket or
trusted network. To encrypt it, pass `--tls` (env `OKF_PG_TLS=true`) or put
`sslmode=require` in the connection string; either negotiates a `rustls` TLS
session that verifies the server certificate against the platform trust store.
`sslmode=disable`/`prefer` (or an omitted `sslmode`) keep the plaintext default.

## Wiring it into an MCP client

Launch the binary as a stdio MCP server. For a Claude Desktop / Claude Code
style client config:

```json
{
  "mcpServers": {
    "pgokf": {
      "command": "/path/to/pgokf-mcp",
      "args": ["--database-url", "postgresql://okf_reader@localhost/app"],
      "env": { "OKF_PG_URL": "postgresql://okf_reader@localhost/app" }
    }
  }
}
```

The agent then sees the tools above and can search, expand, and read the
catalog. Prefer supplying the connection string through `OKF_PG_URL` in `env`
rather than on the command line.

## Serving it over HTTP

Some clients cannot launch a subprocess: a hosted agent, a browser-based
client, a fleet of agents that should share one connection to the catalog.
`--http <addr>` serves the same JSON-RPC messages over HTTP - the same
implementation answers both transports, so they cannot drift apart.

```sh
pgokf-mcp --http 127.0.0.1:8081
```

The endpoint is `POST /mcp`. This is the MCP **Streamable HTTP** transport with
the parts a request/response server does not need left out: the server never
speaks first, so it opens no event stream and issues no session id, and
`GET`/`DELETE` on the endpoint answer `405` as the specification allows. The
`initialize` handshake answers with the revision the client asked for when it
is one of `2024-11-05`, `2025-03-26`, or `2025-06-18`, and with the newest of
those otherwise. JSON-RPC batches are refused (`-32600`), as the current
revision requires.

`GET /healthz` needs no token and answers `{"status":"ok"}`, or **503**
`{"status":"degraded"}` when the catalog has stopped answering or the tokens
file is no longer being believed - what is wrong goes to the log, not to
whoever found the port. The connection to PostgreSQL is never re-established,
so wire this to a container health check and let the supervisor restart the
process.

Over stdio the client launched the server and already holds the connection
string, so there is nothing to authenticate. Over HTTP the server is
**reachable**, so:

- **Every request carries a bearer token.** No token, an unknown token, or a
  token anywhere but the `Authorization: Bearer` header is `401`. A token in a
  query string would be written to every access log between the client and
  here, so it is not read from one. The check runs before the request body is
  read and before a request takes one of the server's working slots, so an
  anonymous caller cannot make this server buffer anything or queue anything.
- **The token's role decides which tools it may call.** `tools/list` shows only
  those tools - and only the arguments the caller may use - so a client is
  never offered something it cannot use, and `tools/call` refuses the rest with
  JSON-RPC error `-32001`. A tool that does not exist is reported as unknown,
  not as a refusal.
- **A request carrying a browser `Origin` is refused** unless the operator
  named that origin in `--allowed-origins`. This is what stops a page in
  someone's browser from reaching a server on their private network. A named
  origin also gets the CORS answers a browser needs (preflight, and the
  response marked readable); naming none allows no browser anything.
- **`output_dir` and `overwrite` are refused** (`-32602`): they would write the
  built plugin onto the *server's* filesystem, not the caller's. Leave them out
  and the files come back inline in the result (up to 1 MiB; above that, narrow
  the selection).
- **Requests are bounded**: 1 MiB of body, 15 seconds to send it, 32 worked on
  at once across every connection, 60 seconds each, and a statement timeout on
  the catalog under that so a query the caller gave up on does not keep
  running.
- **TLS is not terminated here.** Bind it to the loopback interface and put a
  reverse proxy in front of it, or keep it on a private network; the server
  says so at startup if the address it binds is reachable from elsewhere.
- **One process serves one tenant.** `--tenant` scopes the single catalog
  session, and only tokens minted for that tenant are admitted (see below).
  Run one server per tenant, with tokens of its own.

Authentication is a static token, not OAuth. A `401` says
`WWW-Authenticate: Bearer realm="pgokf-mcp", error="invalid_token"` rather than
pointing at an authorization server, so a client should be configured with the
token rather than left to discover one.

### Tokens and roles

Tokens are minted on the catalog's Admin page (`pgokf-web`, signed in as an
admin) or, without the UI, with `pgokf-web mcp-token mint --name fleet --role
reader` against the writer URL. Only the SHA-256 digest is stored, in the
catalog's `pgokf_web.mcp_tokens`, so the token is shown once and nothing can
recover it later. `name` is what appears in this server's log beside every call
the token makes.

This server connects as a reader, and a reader cannot see that table. It hashes
the token a request presents - only if it has the shape a minted token has - and
asks `pgokf.mcp_token_bearer(digest)`, a `SECURITY DEFINER` lookup that answers
for the one digest it is given and lists nothing: the token never travels to
the database, and the reader learns nothing about tokens it does not hold.
Revoking a token on the Admin page (or with `pgokf-web mcp-token revoke --name
fleet`) takes effect with the next request; there is no file to re-read and no
cache to expire. The lookups run on a connection of their own, so they never
wait behind a tool call, and are bounded in number and in wait, so a flood of
wrong tokens costs the catalog a fixed amount; at startup the server proves the
lookup exists, so a catalog older than 0.2.0 is refused with the reason rather
than answering 503 later. A token is minted for the tenant of the UI (or
`pgokf-web --tenant`) that minted it, and this server admits only tokens minted
for the tenant it serves (`--tenant`, or none): one process serves one tenant,
with tokens of its own.

Only tokens the catalog minted are accepted: the server checks a presented
token's shape (`pgokf_` and 43 base64url characters, 256 bits of randomness)
before it hashes it. That is what makes one fast unsalted hash per request the
right choice rather than a slow one, so a row holding the digest of a chosen
password could never authenticate - and the table's own constraints refuse a
digest that is not 64 hex characters or a role that is not `reader` or
`builder`.

**Revoking** is one click on the Admin page, or `pgokf-web mcp-token revoke`.
It is a `DELETE` in the catalog, and the server asks the catalog on every
request, so it takes effect with the next one. While the catalog cannot be
asked at all, a request is answered `503` with `Retry-After` rather than `401`,
so a client does not discard a good token over an outage; `/healthz` reports
the same condition, so a supervisor watching it sees `degraded` for the whole
outage even during a quiet period with no other traffic.

| Role | May call |
| --- | --- |
| `reader` | `concept_search`, `find_similar`, `concept_neighbors`, `get_concept`, `get_skill` |
| `builder` | everything a reader may, and `list_plugin_targets`, `build_workspace_plugin` |
| `writer` | everything a builder may, and `list_bundles`, `put_document`, `delete_document` |
| `admin` | everything a writer may, and `create_content_bundle`, `refresh_bundle`, `set_bundle_state` |

Roles are a ladder, least first, and `tools/list` shows a token only the tools
its role may call. `reader` and `builder` are read-only against the catalog;
the distinction between them is that building a plugin reads every selected
source through the audited readers and returns the whole tree, so it leaves a
much longer trail than a search.

`writer` and `admin` change the catalog, and need this server to hold a
`pgokf_writer` connection of its own (`--writer-url`). Without one their tools
are still listed - so an agent discovers them and is told plainly that this
endpoint does not write - and every call answers with that. Two rules hold
whatever writes:

- **A contribution arrives unverified.** Whatever a document claims under
  `verified` is set aside under `superseded_verifications`, with who set it
  aside and why, and `generated` names the token as `agent:<token name>`. A
  verification is granted by an approver reviewing the document in the web UI;
  it is never something a contributor - a person or an agent - can type into
  one. This is the same code the web UI's own upload and edit paths run.
- **A write is a full snapshot, serialized across every writer.**
  `put_document` and `delete_document` read the content bundle, change the one
  entry, and write all of it back through `pgokf.register_bundle_content`. The
  read and the write run in one transaction holding a PostgreSQL advisory lock
  on the bundle's name, so a second instance of this server, or the web UI,
  cannot interleave and drop what the other wrote.
- **A write that could not put the bundle back as it found it is refused**, in
  full, with nothing half-written. That covers a catalog that does not keep
  document sources (`store_source` off); a bundle carrying an `index.md` or a
  `log.md`, whose bytes the catalog does not keep and which a rewrite would
  drop; a bundle larger than this server will hold in memory at once; a name
  that does not resolve to the row that was read (which happens on a session
  not scoped to a tenant, where a name may be taken in several - start the
  server with `--tenant`); a path inside a skill package, which is served to
  agents whole; and a document declaring that a person who is not the
  contributor produced it.

Two things are deliberately **not** exposed here, and stay with a person at the
web UI or at `psql`: `unregister_bundle`, which deletes every concept of a
bundle irreversibly, and registering a filesystem bundle, whose path is read by
the database server and is an operator's to choose. `set_bundle_state` covers
the reversible half (enable, disable, retire, bring back).

### Wiring an HTTP client to it

```json
{
  "mcpServers": {
    "pgokf": {
      "type": "http",
      "url": "https://catalog.example/mcp",
      "headers": { "Authorization": "Bearer pgokf_..." }
    }
  }
}
```

Or by hand:

```sh
curl -s https://catalog.example/mcp \
  -H "Authorization: Bearer $PGOKF_MCP_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

A plugin built by `build_workspace_plugin` configures the **stdio** form by
default (`pgokf-mcp --env-file ${PLUGIN_DATA}/pgokf.env`), which is what a
locally installed plugin wants. Pass `mcp_url` and it writes the remote form
instead, in the harness's own documented shape, with the token referenced
rather than written - see the
[builder's README](../pgokf-workspace/README.md#pointing-at-an-http-endpoint).

## Scripted stdio session (and how to test it)

Because the transport is newline-delimited JSON-RPC, you can drive the server by
piping JSON-RPC lines into it. Each request line yields one response line;
notifications (no `id`) get no reply.

```
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"concept_search","arguments":{"query":"handbook","limit":5}}}
```

Piping those three lines into `pgokf-mcp --database-url ...` returns, in order:
the `initialize` result (with `serverInfo` and `capabilities`), the `tools/list`
result (a `tools` array), and the `tools/call` result (an `isError:false` tool
result whose text is the JSON search hits from the live catalog). This is
exactly the end-to-end check the release runs.
