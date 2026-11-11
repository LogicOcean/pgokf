# Confluence integration

`pgokf-confluence` imports a Confluence Cloud space into the existing pgokf
catalog. It is a native Rust companion in this workspace, shipped in the
companions image, with dry-run export and explicit application to PostgreSQL. Confluence remains the editable wiki;
pgokf supplies the searchable OKF projection and its existing MCP access.

This pilot supports a **shared audience**: every reader of the destination
catalog must be allowed to read every imported page. It does not preserve
Confluence page ACLs. Select the source space and destination access boundary
accordingly before enabling recurring ingestion.

## How the data moves

```text
Confluence Cloud space
    → complete page inventory and storage-body reads
    → Markdown + OKF source metadata
    → complete, validated bundle snapshot
    → pgokf.register_bundle_content_with_context
    → catalog search, links and existing MCP tools
```

The companion performs the HTTPS requests. The PostgreSQL extension receives
only the converted document bytes and metadata. There is no additional SQL
schema or Confluence credential stored in the database. The integration does
not expose an HTTP listener; run it once yourself or schedule it as a job.

## Content and identity

A page becomes a `Document` concept at `pages/<page-id>.md`. Renaming a
page does not change its identity. Source metadata records the site, space,
page, version and canonical source URL. Conversions are deterministic so that
an unchanged page does not acquire a new catalog version on every run.

The converter handles ordinary text structures plus `code`, `noformat`,
`panel`, `note`, `info`, `tip` and `warning` storage-format macros. Links between pages included in the bundle become
relative links, which pgokf can resolve into graph edges. Links to content
outside the imported space remain source links. Unsupported macro constructs
produce conversion warnings; review those warnings against representative
wiki pages before relying on the imported text. An explicitly empty storage
body is a valid blank page: its identity and source link are retained with a
visible placeholder and warning. A missing body field or malformed XHTML still
fails the entire import.

Attachments remain links to Confluence. Their binary bodies, PDF/Office text,
OCR, comments and a complete history of source versions are outside this
pilot. A catalog reader still needs the original Confluence permissions to
open linked content. The importer does not grant human verification or approval
to converted content.

## Provenance

Each import is one `pgokf.register_bundle_content_with_context` call whose
context labels the change. The catalog stores these values verbatim; they are
opaque labels, not authorization:

| Field | Value |
| --- | --- |
| `producer` | `pgokf-confluence` (unversioned, so event consumers can route on it) |
| `origin` | `confluence:<site origin>:space:<space ID>` |
| `manifest_hash` | BLAKE3 of every document path and content hash |
| `observed_source_generation` | BLAKE3 of every page ID and version |

The catalog-change outbox event of every import records the producer and
origin, so a dispatcher can tell a Confluence resync from other writers. A
bundle's freshness row is created by its first import with this producer and
source generation; see the provenance and freshness sections of the
[SQL API](sql-api.md). The converter version is recorded in each document's
`converter_version` and `generated.by` fields.

## Individual setup

1. Install a PostgreSQL database with the pgokf extension as described in
   [Getting started](getting-started.md). Give a dedicated ingestion login
   membership in `pgokf_writer` and configure its normal authentication.
2. Select a Confluence Cloud space whose audience matches the catalog readers.
   Obtain its numeric space ID, the account email and an API token with the read
   scopes needed for space/page retrieval. Account permissions still apply.
3. Build `cargo build --locked --release -p pgokf-confluence` from the repository
   root. Set the source variables and token file below.
4. Export and inspect a preview, then apply with a persistent checkpoint.

```bash
export CONFLUENCE_URL='https://example.atlassian.net'
export CONFLUENCE_EMAIL='wiki-reader@example.com'
export CONFLUENCE_SPACE_ID='123456'
export OKF_BUNDLE_NAME='confluence-team-wiki'
export CONFLUENCE_TOKEN_FILE='/run/secrets/confluence-api-token'
# Scoped tokens additionally require CONFLUENCE_CLOUD_ID='<site UUID>'.

target/release/pgokf-confluence --output-dir /tmp/team-wiki-preview

export OKF_PG_URL='postgresql://confluence_ingest@db.internal/catalog?sslmode=require'
mkdir -p "$HOME/.local/state/pgokf-confluence"
target/release/pgokf-confluence \
  --apply --tls \
  --state-file "$HOME/.local/state/pgokf-confluence/team-wiki.json"
```

Create the token file through your secret manager or another private method;
restrict access to the ingestion account. Alternatively set
`CONFLUENCE_API_TOKEN` in the process environment. Do not configure both token
sources. A token value is deliberately not accepted as a CLI argument.
The database URL above illustrates the host/login only; provide the database
credential through the protected runtime environment when required.

Scoped tokens require a Cloud ID and use Atlassian's API gateway. Classic
API-token access uses the canonical site. Before a scoped-token crawl, the
companion reads the site's public tenant record (`/_edge/tenant_info`, without
credentials) and refuses a Cloud ID that belongs to another site. The source
identity is the site and the space alone, so moving the same space between a
classic and a scoped token keeps its bundle and checkpoint. The account's
permissions and token scopes both constrain reads. See
[Atlassian's scoped-token documentation](https://support.atlassian.com/confluence/kb/scoped-api-tokens-in-confluence-cloud/).
Data Center and corporate vanity-domain installations are not supported by
this Cloud adapter.

The export directory must not already exist. It contains the converted
`pages/<page-id>.md` files and a `manifest.json` snapshot. A preview does not
connect to PostgreSQL or update the checkpoint, even when `OKF_PG_URL` is set;
it is not a database-connectivity or authorization test.

Use a separate checkpoint file and bundle name for each source space. Retain
the checkpoint across runs. For later deletion reviews, provide the same
`--state-file` used by apply:

```bash
target/release/pgokf-confluence \
  --state-file "$HOME/.local/state/pgokf-confluence/team-wiki.json" \
  --output-dir /tmp/team-wiki-review-1
```

The JSON result reports `removed_page_ids` against that checkpoint. Without a
checkpoint, `deletion_baseline` is `none`; an empty removal list then says
nothing about changes to an existing import. Export directories contain wiki content and need
the same access controls as the destination catalog. See the
[companion README](https://github.com/LogicOcean/pgokf/tree/main/crates/pgokf-confluence)
for the complete option table.

## Snapshot and deletion rules

Each run inventories pages, fetches their bodies and checks the final inventory
for consistency. It fails instead of committing a partial crawl, an empty
space or a page set above the configured limit. Bounded HTTP retries handle
retryable failures; terminal errors leave the catalog snapshot unapplied.
The pilot does not persist an incremental body cache.

`register_bundle_content_with_context`, like `register_bundle_content`, treats
each call as the entire bundle: omitted concepts are removed. The companion therefore makes one atomic bundle call,
never a call per page or pagination batch. Its checkpoint is tied to the
source/destination identity and advances only after a successful ingestion.
It refuses to adopt an unrelated existing bundle. A supplied state file is
locked for the entire invocation, including previews. Apply also holds the
native per-bundle PostgreSQL advisory lock across the source crawl and database
transaction. The lock is released when the session ends. Its lock key is shared
across tenants for the same bundle name, so overlapping imports with the same
name can block each other even when their tenant selectors differ.

A removed page, an archived page, a move out of the selected space, or loss of
visibility can all remove an ID from the source inventory. Application blocks
on removals by default. Inspect the reported IDs and their source state before
running `--apply --allow-deletions` with the same source and state file.
`--allow-deletions` is a boolean acknowledgment of removals, not an ID filter;
review a fresh preview immediately before applying. Do not put that option
into the recurring job by default. Empty crawls remain rejected even when
deletions are allowed. Moving a page between spaces requires reconciliation of
both space bundles. Do not delete the checkpoint to bypass a removal or ownership
check.

No paginated API crawl is a transactional snapshot of the remote wiki. The
initial/final consistency checks catch observed changes; they do not guarantee
that a page cannot change immediately afterward. Retry a failed crawl and
choose a schedule appropriate to the space's editing rate.

Inside the verified transaction, immediately before COMMIT, the importer
durably writes the snapshot it is committing to `<state-file>.pending`; after
the commit it renames that file over the checkpoint. A crash, a full disk or a
lost commit reply can leave the pending file behind. The next `--apply`
settles it under the bundle lock before crawling: if the catalog holds the
pending snapshot, the commit happened and the pending file becomes the
checkpoint; if the catalog still holds the last checkpoint, the commit did not
happen and the pending file is discarded. Either way the run continues and
measures removals against what the catalog actually holds, even when pages
changed in the meantime. The JSON result reports `pending_checkpoint` as
`promoted`, `discarded`, or (in a preview, which cannot consult the catalog)
`unresolved`. A catalog that matches neither snapshot changed outside the
importer, and the run refuses it. Keep the pending file with the checkpoint;
never delete either to bypass a refusal.

## Access control and retained content

The integration reads as one Confluence account. It does not map Confluence
users/groups, inherited page restrictions or permission revocations into
pgokf authorization. `--tenant` selects catalog scope (default `default`); it does not establish a
Confluence permission boundary. The [multi-tenancy guide](multi-tenancy.md)
explains why clients with arbitrary SQL access can change the tenant selector.

If a previously shared page becomes restricted, stop serving the affected
catalog content while its visibility is reconciled. A failed import preserves
the prior snapshot, which can be stale relative to a source access change.
Do not interpret a stopped or failing synchronization job as revocation of
access to content already stored in PostgreSQL.

When history is enabled, removing a current concept does not remove its
historical versions. Plan retention for catalog rows, stored source, history,
derived representations, preview exports and backups. See
[Version history](version-history.md) and [Security](security.md).

## Team deployment

Use an **Azure Container Apps Job** to run the same one-shot companion on a
schedule. The example has no ingress, references secrets through Key Vault,
and mounts Azure Files for the checkpoint. It runs the `pgokf-confluence`
binary from the companions image; PostgreSQL is an existing separate service
with pgokf installed.

Follow the repository's
[Confluence container deployment example](https://github.com/LogicOcean/pgokf/tree/main/deploy/confluence).
Begin with the supplied dry-run configuration, inspect an execution, and add
`--apply` when the source and destination are ready. Retain the same durable
checkpoint path for subsequent executions. Monitor failures and stale data;
a scheduled job is not a guarantee that every scheduled ingestion succeeded.

## Pilot limits

- Cloud, one selected space and one complete nonempty snapshot per invocation.
- Shared-audience access only; no per-user Confluence ACL enforcement.
- Basic storage-format conversion, macro warnings and attachment links.
- Full source reads on each run, bounded by page/response limits and available
  memory; no incremental fetch cache or webhook processing.
- Read-only Confluence operations; no publication, page creation or two-way sync.

The converter limits each page to 100,000 rendered table cells, counted across
all tables and including padding and Markdown header separators. It checks a
table's shape before rendering, so a wide row followed by many empty rows cannot
force a large rectangular allocation. The entire rendered OKF document,
including metadata, is limited to 4 MiB. A limit violation stops the complete
import with a conversion error before any catalog write; it does not truncate
or omit the offending page.

The whole bundle travels to PostgreSQL in one protocol message, which the
server caps at 1 GiB. `--max-snapshot-bytes` (default 512 MiB, at most
960 MiB) bounds the converted space - document bytes plus paths - and is
checked in previews too, so a dry run never reports a snapshot that apply
could not register. The database's `pgokf.max_bundle_files` and
`pgokf.max_file_bytes` limits also apply. Raise limits only after measuring the source and memory needs. Use
conversion fixtures and a test space to assess suitability before widening
the pilot's scope.
