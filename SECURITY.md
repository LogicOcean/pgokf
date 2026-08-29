# Security Policy

## Supported versions

`pgokf` is pre-1.0. Security fixes are made against the latest released
`0.1.x` line; older tags do not receive backports. Upgrade to the newest
release to receive fixes.

| Version | Supported |
| ------- | --------- |
| latest `0.1.x` | ✅ |
| older `0.1.x` | ❌ |

## Reporting a vulnerability

**Please do not open a public GitHub issue for a security vulnerability.**

Report it privately through one of:

- GitHub's **[Private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)**
  (the **Security → Report a vulnerability** button on this repository), or
- email **security@logicocean.example** *(replace with the real address before
  publishing)*.

Please include: the affected version/commit, a description of the issue, and a
minimal reproduction if you have one. We aim to acknowledge a report within a
few business days and to coordinate a fix and disclosure timeline with you.

## Scope and threat model

pgokf runs inside PostgreSQL and treats a **filesystem path** and **bundle
content** as privileged, untrusted input. The security model - role tiers,
`SECURITY DEFINER` hardening, path-traversal/symlink defenses, resource
ceilings, and the multi-tenancy trust model (including the important caveat that
the `pgokf.tenant` GUC is a scoping selector, **not** a hard boundary against a
tenant who can run arbitrary SQL) - is documented in
[`docs/security.md`](docs/security.md) and
[`docs/multi-tenancy.md`](docs/multi-tenancy.md). Read those before deploying in
a multi-tenant or untrusted-input setting.

The companion tools (`pgokf-ingest`, `pgokf-embed`, `pgokf-mcp`) hold
object-store / embedding-endpoint credentials in their own environment and never
send them to PostgreSQL; they support TLS to PostgreSQL via `--tls` /
`sslmode=require`.
