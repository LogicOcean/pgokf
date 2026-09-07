# pgokf-mcp

A **Model Context Protocol** server that exposes the [`pgokf`](../extension)
catalog to AI agents as MCP tools.

`pgokf-mcp` is a standalone async binary that speaks MCP over **stdio**:
newline-delimited JSON-RPC 2.0 on stdin/stdout. It implements the MCP handshake
(`initialize` → `serverInfo`/`capabilities`, then `tools/list` and `tools/call`)
and backs each tool with a query against the shipped `pgokf` public functions.
The JSON-RPC layer is **hand-rolled on `serde_json`** - no MCP SDK dependency,
so it adds nothing new to the workspace's `cargo deny` surface.

## Tools

| Tool | Arguments | Backed by |
| --- | --- | --- |
| `concept_search` | `query` (required), `bundle_id?`, `limit?`, `type?`, `tags?`, `status?`, `trust_tier?` | `pgokf.concept_search` |
| `find_similar` | `concept_id` (required), `bundle_id?`, `limit?` | `pgokf.find_similar` |
| `concept_neighbors` | `concept_id` (required), `max_hops?`, `bundle_id?` | `pgokf.concept_neighbors` |
| `get_concept` | `concept_id` (required), `bundle_id?` | `pgokf.concepts` projection |
| `get_skill` | `bundle_id` (required), `concept_id` (required) | `pgokf.get_skill`: an Agent Skills package stored in the catalog (metadata, the exact `SKILL.md` text, and the scripts, references, and assets it owns); audited |
| `list_plugin_targets` | none | the `pgokf-workspace` target registry: each harness's kind and documented directory |
| `build_workspace_plugin` | `target` (required; `agent-plugin` for a portable Agent Plugins 1.0.0 directory, a harness id, or `custom` with `harness` `{label, kind?, skills_dir?}` for an agent the registry does not know), `name?`, `title?`, `all?` (start from every visible concept, so a selection narrowed only by types, tags, or a query needs no bundle), `bundle_ids?`, `concept_ids?`, `picks?` (`"bundle_id:concept_id"` strings: specific files added to whatever else matches), `tags?`, `types?`, `query?`, `verified_only?`, `limit?`, `base_model?`, `components?` (`mcp`, `guide`, `tools`; default all), `mcp_command?`, `web_url?`, `output_dir?`, `overwrite?` | `pgokf-workspace`: an Agent Skills package (`SKILL.md` + `references/`), an `AGENTS.md` instruction file, an Ollama prompt bundle, or a generic tree, with `okf-workspace.yaml` and `okf-workspace.lock`; skill packages stored in the catalog (`type: Skill`, select them with `types: ["Skill"]` or by id) are copied whole and byte for byte, scripts executable; with `output_dir` the tree is written into the workspace, otherwise the files are returned inline (up to 1 MiB; UTF-8 files as text, binary files as `{"encoding": "base64", "data": ...}`) |

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
| `--tenant` | `OKF_TENANT` | Apply a `pgokf.tenant` scope for the session (multi-tenant isolation; required once the catalog's `require_tenant` policy is on) |
| `--tls` | `OKF_PG_TLS` | Require a TLS-encrypted link to PostgreSQL (default off) |

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
