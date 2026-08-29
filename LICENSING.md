# Licensing

`pgokf` is **dual-licensed**.

## The open-source license (AGPL-3.0)

The **core** — the PostgreSQL extension and its supporting crates
(`crates/extension`, `crates/okf-parser`, `crates/okf-sync`) — is licensed under
the **GNU Affero General Public License, version 3.0 only** (`AGPL-3.0-only`).
The full text is in [`LICENSE`](LICENSE).

In plain terms, under the AGPL you may use, study, modify, and redistribute
pgokf for free — including inside your own company — **provided that** if you
distribute a modified version, **or make a modified version available to others
over a network** (for example, offering it as a hosted or managed service), you
also make the complete corresponding source of your version available under the
AGPL. This "network use is distribution" clause is the difference between the
AGPL and the ordinary GPL.

Using pgokf unmodified inside your organization, or building applications that
merely *connect to* a PostgreSQL server with pgokf installed, does not by itself
obligate you to release your application's source. If in doubt, consult a
lawyer — this section is a summary, not legal advice, and the text in
[`LICENSE`](LICENSE) governs.

## The companion tools (MIT)

The standalone companion binaries are **not** copyleft — they are licensed under
the permissive **MIT** license (see [`LICENSE-MIT`](LICENSE-MIT)) so they can be
embedded freely in any pipeline, open or proprietary:

- `crates/pgokf-ingest` — mountless object-store ingestion
- `crates/pgokf-embed` — the reference embedding companion
- `crates/pgokf-mcp` — the MCP server
- `crates/pgokf-pgconn` — the shared TLS connection helper

## The commercial license

If the AGPL does not fit your needs — for example you want to **embed the core
into a proprietary product**, **offer pgokf as a managed/hosted service without
releasing your modifications**, or your organization has a policy against AGPL
software — a **separate commercial license** is available that removes the AGPL
copyleft obligations.

To obtain a commercial license, contact **LogicOcean** at
`licensing@logicocean.example` *(replace with the real contact before
publishing)*.

## Contributing

Because pgokf is offered under both the AGPL and a commercial license, external
contributions require a **Contributor License Agreement (CLA)** so that the
project can continue to offer the commercial option. See `CONTRIBUTING.md`
*(to be added)* before opening a pull request.

## Summary

| Component | License |
| --------- | ------- |
| `crates/extension`, `crates/okf-parser`, `crates/okf-sync` (the core) | AGPL-3.0-only, or a commercial license |
| `crates/pgokf-ingest`, `crates/pgokf-embed`, `crates/pgokf-mcp`, `crates/pgokf-pgconn` (companions) | MIT |

`SPDX-License-Identifier` headers in each source file record the applicable
license per file.
