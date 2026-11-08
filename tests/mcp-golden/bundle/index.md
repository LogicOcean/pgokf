# Golden MCP fixture bundle

This is a reserved OKF bundle file (`index.md`). It is intentionally **not**
ingested as a concept: `pgokf` skips `index.md` and `log.md` at every
directory level.

The documents in this bundle seed the golden legacy MCP wire fixtures captured
by `tests/test_mcp_golden.py`:

- `runbooks/failover` - a failover runbook linking to the service and appendix
- `runbooks/failover-appendix` - the appendix, linking back to the runbook
- `services/widget` - a tagged service document
- `skills/deploy` - one Agent Skills package (SKILL.md, one script, one
  reference)

The fixture catalog is registered from this directory with
`pgokf.register_bundle` by the harness; nothing here is machine-specific.
