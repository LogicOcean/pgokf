# pgokf-mcp

A **Model Context Protocol** server that exposes the [`pgokf`](../extension)
catalog to AI agents as MCP tools.

`pgokf-mcp` is a standalone async binary that speaks MCP over **stdio**:
newline-delimited JSON-RPC 2.0 on stdin/stdout. It implements the MCP handshake
(`initialize` → `serverInfo`/`capabilities`, then `tools/list` and `tools/call`)
and backs each tool with a query against the shipped `pgokf` public functions.
The JSON-RPC layer is **hand-rolled on `serde_json`** — no MCP SDK dependency,
so it adds nothing new to the workspace's `cargo deny` surface.

## Tools

| Tool | Arguments | Backed by |
| --- | --- | --- |
| `concept_search` | `query` (required), `bundle_id?`, `limit?`, `type?`, `tags?`, `status?`, `trust_tier?` | `pgokf.concept_search` |
| `find_similar` | `concept_id` (required), `bundle_id?`, `limit?` | `pgokf.find_similar` |
| `concept_neighbors` | `concept_id` (required), `max_hops?`, `bundle_id?` | `pgokf.concept_neighbors` |
| `get_concept` | `concept_id` (required), `bundle_id?` | `pgokf.concepts` projection |

Each tool returns an MCP tool result whose single text content block holds the
JSON array of rows exactly as the SQL function produced them.

## Configuration

| Flag | Env | Meaning |
| --- | --- | --- |
| `--database-url` | `OKF_PG_URL` | PostgreSQL URL for a `pgokf_reader`-capable role (required) |
| `--tenant` | `OKF_TENANT` | Apply a `pgokf.tenant` scope for the session (multi-tenant isolation) |

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

The agent then sees the four tools above and can search, expand, and read the
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
