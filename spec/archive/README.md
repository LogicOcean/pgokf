# Archived: the standalone "Code Catalogue System" service

`code-catalogue-service-SPEC.md` and its OpenAPI document describe a separate
Axum/REST service with its own entries, versions, releases, ACL and token
model, and an instance-to-instance sync protocol. On 2026-09-05 the decision
was taken that there is one product: the pgokf extension (with its companions
and the MCP server) **is** the catalogue, and no second data model is kept.

These files are retained for reference only. The parts that remain normative
were carried over:

- CTL1 grammar, JSON schemas, and golden fixtures now live in `../ctl1/` and are
  cited by `../OKF-EXTENSION-SPEC.md` (§4.5, §4.7).
- The search and security clauses of Appendix C inform §7 and §11 of the
  extension spec; where they conflict, the extension spec wins.

The thin HTTP/UI layer, if built, is specified by §10 of the extension spec and
by the MCP server's tool contract; a new OpenAPI document is generated from
that surface when it exists.
