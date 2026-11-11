# pgokf-confluence

A one-shot Confluence Cloud ingestion companion for [pgokf](../extension).
It reads an explicitly selected space, converts published pages into an OKF
bundle, and can submit the complete bundle to PostgreSQL. Confluence remains
the source of truth. The extension performs no Confluence network requests and
receives no Confluence token. The binary ships in the companions image
(`ghcr.io/logicocean/pgokf-companions:<version>`) alongside `pgokf-ingest`,
`pgokf-embed`, `pgokf-mcp` and `pgokf-web`.

This is a **shared-audience pilot**: use a space whose content may be read by
all readers of the destination catalog. The companion does not reproduce
Confluence page permissions. Attachments remain links; supported macro content
is converted and unsupported macros produce warnings. There is no Data Center
adapter, continuous watcher, or publishing back to Confluence.

## Build and preview

From the repository root:

```bash
cargo build --locked --release -p pgokf-confluence
export CONFLUENCE_URL='https://example.atlassian.net'
export CONFLUENCE_EMAIL='wiki-reader@example.com'
export CONFLUENCE_SPACE_ID='123456'
export OKF_BUNDLE_NAME='confluence-team-wiki'
export CONFLUENCE_TOKEN_FILE='/run/secrets/confluence-api-token'
# For scoped API tokens, also set CONFLUENCE_CLOUD_ID to the site UUID.

target/release/pgokf-confluence --output-dir /tmp/confluence-preview
```

Provide the token through a protected file or `CONFLUENCE_API_TOKEN`; there is
no token-value command-line argument. Preview is the default and needs no
PostgreSQL connection. The output directory must not already exist. Each export contains
`pages/<page-id>.md` files and `manifest.json`. Inspect the converted pages and
warnings before ingestion.

## Apply

Provision an existing PostgreSQL login with membership in `pgokf_writer`, then
supply its connection string through the process environment or secret manager:

```bash
export OKF_PG_URL='postgresql://confluence_ingest@db.internal/catalog?sslmode=require'
mkdir -p "$HOME/.local/state/pgokf-confluence"
target/release/pgokf-confluence \
  --apply \
  --tls \
  --state-file "$HOME/.local/state/pgokf-confluence/team-wiki.json"
```

The login still needs its normal database authentication; the example URL
contains no password. `--apply` requires both a database URL and a persistent
state file. Reuse that file for subsequent runs of the same source and bundle.
The companion rejects adoption of an unrelated existing bundle. A failed or
inconsistent crawl is not submitted. The checkpoint advances only after a
successful catalog update. Preview never connects to PostgreSQL or updates the
checkpoint. Pass the existing `--state-file` on later previews to obtain an
accurate `removed_page_ids` comparison; without it there is no deletion
baseline. Removed page IDs block application by default. Review a fresh preview
before using the boolean `--allow-deletions` flag on a deliberate apply run.

## Configuration

Run `pgokf-confluence --help` for the exact CLI. Like every companion, an
optional value that is set but empty in the environment counts as unset. The
principal options are:

| Option | Environment | Purpose |
| --- | --- | --- |
| `--site-url` | `CONFLUENCE_URL` | Canonical HTTPS `*.atlassian.net` site URL |
| `--email` | `CONFLUENCE_EMAIL` | Atlassian account associated with the token |
| Token value: environment only | `CONFLUENCE_API_TOKEN` | API token; do not also configure a token file |
| `--token-file` | `CONFLUENCE_TOKEN_FILE` | File containing the API token |
| `--cloud-id` | `CONFLUENCE_CLOUD_ID` | Site UUID required for scoped-token gateway routing |
| `--space-id` | `CONFLUENCE_SPACE_ID` | Numeric space ID to import |
| `--bundle-name` | `OKF_BUNDLE_NAME` | Stable destination content-bundle name |
| `--database-url` | `OKF_PG_URL` | PostgreSQL connection URL; required for apply |
| `--tls` | `OKF_PG_TLS` | Require certificate-verified PostgreSQL TLS; a weaker `sslmode` is raised to `require` |
| `--tenant` | `OKF_TENANT` | Catalog tenant scope, default `default`; not a Confluence ACL |
| `--state-file` | — | Durable source-bound checkpoint; required for apply, supplies deletion baseline for preview |
| `--output-dir` | — | Export converted OKF files and manifest to a directory that does not exist |
| `--apply` | — | Enable the catalog write; omitted means preview |
| `--allow-deletions` | — | Boolean flag authorizing the removed IDs reported by preview; default refuses any removal |
| `--max-pages` | — | Maximum pages per crawl; default 10,000 |
| `--max-response-bytes` | — | Per-response bound; default 8 MiB |
| `--max-retries` | — | Maximum retries per read request; default 3, maximum 10 |

The crawler inventories the space before and after fetching pages. It requires
a nonempty, consistent inventory and sends one complete snapshot to
`register_bundle_content_with_context`. Conversion caps each page at 100,000
rendered table cells (including padding and header separators) and 4 MiB of
output including metadata. Oversized content fails the complete import before
any catalog write. Apply holds the native per-bundle PostgreSQL lock across the
crawl and transaction. A supplied state file is locked throughout either mode.
It does not cache unchanged page bodies yet: each run reads the source again,
while pgokf hashes converted files to avoid rewriting unchanged catalog
concepts. A continuously edited space may require a later retry to obtain a
consistent snapshot.

Every import carries a provenance context that the catalog-change outbox
records and a new bundle's freshness row starts from: `producer` is
`pgokf-confluence`, `origin` is `confluence:<site>:space:<space-id>`,
`manifest_hash` is a BLAKE3 hash of the exact document set, and
`observed_source_generation` is a BLAKE3 hash of every page ID and version, so
it moves on any source edit even when the converted Markdown is unchanged.

See the [integration guide](../../docs/confluence-integration.md) for content
mapping, permissions, failure handling and individual operation. The
[container deployment example](../../deploy/confluence/README.md) includes an
Azure Container Apps Job with persistent state and Key Vault secret references.

## Tests

Run the offline client, converter, checkpoint and CLI suite from the repository root:

```bash
cargo test --locked -p pgokf-confluence
```

The catalog workflow is ignored by that command because it needs PostgreSQL
with this checkout's pgokf extension installed. This runner creates a private
local cluster, grants only `pgokf_writer` to the test login, runs the workflow,
and stops and removes the cluster afterward:

```bash
PGOKF_TEST_PG_CONFIG=/usr/lib/postgresql/18/bin/pg_config \
  bash scripts/test-confluence-catalog.sh
```

It tests initial import, unchanged and updated pages, deletion approval,
foreign-bundle refusal, writer overlap, checkpoint recovery, transactional
rollback of invalid OKF, and the provenance a first import records. CI runs
this workflow on PostgreSQL 18, and the companions-image smoke test runs the
packaged binary. No Confluence credentials are needed for these tests.
