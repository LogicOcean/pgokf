# Licensing

`pgokf` is **dual-licensed**: an open-source license (AGPL-3.0) for the whole
project, and a separate **commercial license** for use the AGPL does not permit.

## The open-source license (AGPL-3.0)

**Every crate in this repository** — the PostgreSQL extension, its supporting
libraries (`okf-parser`, `okf-sync`), and the companion tools (`pgokf-ingest`,
`pgokf-embed`, `pgokf-mcp`, `pgokf-pgconn`) — is licensed under the **GNU Affero
General Public License, version 3.0 only** (`AGPL-3.0-only`). The full text is in
[`LICENSE`](LICENSE), and every source file carries an
`SPDX-License-Identifier: AGPL-3.0-only` header.

In plain terms, under the AGPL you may use, study, modify, and redistribute
pgokf for free — including inside your own organization — **provided that** if
you distribute a modified version, **or make a modified version available to
others over a network** (for example, offering it as a hosted or managed
service), you also make the complete corresponding source of your version
available under the AGPL. This "network use is distribution" clause is what
separates the AGPL from the ordinary GPL.

Using pgokf unmodified inside your organization, or building applications that
merely *connect to* a PostgreSQL server with pgokf installed, does not by itself
obligate you to release your application's source. If in doubt, consult a
lawyer — this section is a summary, not legal advice, and the text in
[`LICENSE`](LICENSE) governs.

## The commercial license

If the AGPL does not fit your needs — for example you want to **embed pgokf into
a proprietary product**, **offer it as a managed/hosted service without releasing
your modifications**, or your organization has a policy against AGPL software — a
**separate commercial license** is available that removes the AGPL copyleft
obligations.

To obtain a commercial license, contact **LogicOcean** at
`licensing@logicocean.example` *(replace with the real contact before
publishing)*.

## Contributing

Because pgokf is offered under both the AGPL and a commercial license, external
contributions require a **Contributor License Agreement (CLA)** so that the
project can continue to offer the commercial option. See
[`CONTRIBUTING.md`](CONTRIBUTING.md) before opening a pull request.
