# Confluence companion container deployment

This example runs the `pgokf-confluence` binary from the pgokf companions
image in an Azure Container Apps Job. The job reads Confluence and connects to
an existing PostgreSQL database. It has no inbound port or MCP listener.
See [the integration guide](../../docs/confluence-integration.md) for the
shared-audience restriction and deletion rules.

## The image

The companions image (`ghcr.io/logicocean/pgokf-companions:<version>`, built
from [`packaging/docker/Dockerfile.companions`](../../packaging/docker/Dockerfile.companions))
contains `pgokf-confluence` next to the other companions. It has no
entrypoint, so name the binary as the command. It runs as UID/GID `10001`,
contains CA certificates, and needs a writable persistent directory for apply
mode. Build it locally from the repository root, or mirror a published tag
into your own registry:

```bash
docker build -f packaging/docker/Dockerfile.companions -t pgokf-companions:local .
docker run --rm pgokf-companions:local pgokf-confluence --help
```

For a local dry run, provide the configuration variables in your environment
and a protected token file. Docker forwards the named environment variables;
no token value is placed in the command:

```bash
docker run --rm \
  --env CONFLUENCE_URL --env CONFLUENCE_EMAIL \
  --env CONFLUENCE_SPACE_ID --env OKF_BUNDLE_NAME \
  --env CONFLUENCE_CLOUD_ID \
  --mount type=bind,src=/secure/confluence-api-token,dst=/run/secrets/token,readonly \
  pgokf-companions:local pgokf-confluence --token-file /run/secrets/token
```

Make the token readable by the container's UID without making it public.
Omit `CONFLUENCE_CLOUD_ID` for a classic token; an empty value is treated as
unset. For apply, also pass `--env OKF_PG_URL` and `--env OKF_PG_TLS=true`,
mount a writable persistent directory at `/state`, and add
`--apply --state-file /state/team-wiki.json`. Protect that directory and
preserve it between container executions.

## Azure prerequisites

Prepare these resources through your normal infrastructure process:

- A Container Apps environment with network access to Confluence HTTPS,
  PostgreSQL, the image registry, Key Vault and the state file share.
- An existing PostgreSQL service with pgokf installed, trusted TLS and a
  dedicated login granted `pgokf_writer`. The destination readers must have the
  agreed shared audience. Managed PostgreSQL offerings must actually permit
  installing pgokf; ordinary connectivity alone is not sufficient.
- An Azure Container Registry containing the companions image (built from
  this repository or mirrored from GHCR). Use a reviewed image digest in the
  job template.
- A user-assigned managed identity allowed to pull the image and read the two
  required Key Vault secrets. Grant the appropriate registry pull role for
  your registry's permission mode and least-privilege secret access.
- Key Vault secrets named `confluence-api-token` and
  `pgokf-confluence-pg-url`. The second is the database connection URL including
  its required authentication. The template stores references, not values.
- A classic Azure Files share registered on the Container Apps environment
  under the storage name `confluence-state`, with read/write access for the
  job. Keep the checkpoint on this mounted share, never an ephemeral container
  filesystem or `EmptyDir`.

Configure the share using Microsoft's
[Container Apps storage-mount instructions](https://learn.microsoft.com/en-us/azure/container-apps/storage-mounts).
The environment-level storage definition must exist before creating the job.
If using managed identity authentication for an SMB share, that identity is
attached to the **environment**; it is separate from the job identity used for
Key Vault and registry access. Confirm that the selected authentication mode
is supported by your environment and region.

The image runs without root. The sample mount options map the mounted files
to UID/GID `10001`; verify create, replace, rename and file-lock support in the
checkpoint directory before enabling apply. The companion refuses a run if it cannot
acquire the filesystem lock. Restrict the share to the ingestion workload
and operations staff. Preserve it during image updates, job recreation and
credential rotation.

## Create and review the job

Copy `azure-job.yaml.example` to a private working location and replace every
`<...>` placeholder. Use the intended resource IDs, source space, service
account, Cloud ID and immutable image digest. Remove `CONFLUENCE_CLOUD_ID` only
if using a classic token. The supplied configuration runs once per hour in
UTC and starts in dry-run mode. Azure's
[job documentation](https://learn.microsoft.com/en-us/azure/container-apps/jobs)
describes scheduled and manual executions.

```bash
az extension add --name containerapp --upgrade
az containerapp job create \
  --name pgokf-confluence \
  --resource-group <resource-group> \
  --yaml /private/path/confluence-job.yaml

az containerapp job start \
  --name pgokf-confluence \
  --resource-group <resource-group>

az containerapp job execution list \
  --name pgokf-confluence \
  --resource-group <resource-group> \
  --output table
```

These commands create and start Azure resources when you run them. Inspect
the execution logs in your Container Apps environment for the source count,
conversion warnings and failures. Confirm destination access and state-volume
persistence, then add `--apply` to the container's `args` in your working YAML:

```bash
az containerapp job update \
  --name pgokf-confluence \
  --resource-group <resource-group> \
  --yaml /private/path/confluence-job.yaml
```

The next execution can ingest the bundle. Preserve its checkpoint path and
source identity. Do not enable automatic deletions in the scheduled template;
review removal reports and perform a deliberate run when removal is intended.
For tenant-scoped operation, set `OKF_TENANT` and use an appropriately
constrained database access layer. Tenant selection alone
does not enforce Confluence permissions.

The example follows Microsoft's
[Container Apps job YAML schema](https://learn.microsoft.com/en-us/azure/container-apps/azure-resource-manager-api-spec).
It does not create the surrounding environment, identity, registry, database,
Key Vault or file share.

## Schedule, failure and state handling

`parallelism: 1` limits replicas within an execution. It does **not** prevent a
manual execution or another scheduled execution from overlapping it. Use one
job per source/bundle/checkpoint, keep the execution timeout below the schedule
interval, and do not start manual runs while an execution is active. Preserve
the companion's state-file lock and per-bundle PostgreSQL advisory lock; do
not bypass the state lock with a different checkpoint for the same bundle.
The database lock spans the entire apply crawl and catalog transaction; it
does not turn Confluence crawling into a transactional source snapshot. A
preview using `--state-file` also holds the state-file lock, although it does
not connect to PostgreSQL or update the checkpoint. The adjacent `.lock` file
can remain on disk after normal exit; the operating-system lock, not its
presence, determines whether another execution is active. Do not delete it
while a run is active.

The example uses a 30-minute timeout, an hourly schedule and no job-level
retries. The companion still applies its own bounded HTTP retry policy. Tune
these values after measuring the corpus; a timed-out or failed crawl does not
mean the last imported data has been removed. Alert on failed executions,
missing successful imports and deletion/permission warnings.

Back up the checkpoint, and any `.pending` file beside it, together with the
destination catalog's operational state. If a crash occurs between the
database commit and the checkpoint promotion, keep both files and let the next
scheduled execution run: it settles the pending checkpoint against the
catalog, as described in the [integration guide](../../docs/confluence-integration.md#snapshot-and-deletion-rules).
Do not wipe state or rename the bundle to evade ownership checks. Validate the
persistent share and recovery procedure in the pilot before widening the
audience.

Rotation changes Key Vault secrets and runtime configuration, not source
files. Ensure the next execution receives the intended secret version and
verify a dry run after changing source credentials. Changing the credential
must not silently broaden the imported content's audience.
