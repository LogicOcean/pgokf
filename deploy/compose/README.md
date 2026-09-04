# pgokf compose stack

A production-shaped deployment of a pgokf catalog on one Docker host:

- **db** - the pgokf server image (PostgreSQL 18 + pgokf + pgvector +
  pg_cron + pg_search), extensions preloaded, GUC ceilings set, bundles
  bind-mounted read-only at `/bundles`, least-privilege login roles and the
  catalog policy applied on first init.
- **embed** - `pgokf-embed --watch`, embedding every concept that lacks a
  vector on an interval.
- **backup** (profile `ops`) - one-shot verified `pg_dump` + roles dump;
  schedule it from cron.
- **ingest** (profile `ingest`) - `pgokf-ingest --watch` for a bucket-hosted
  bundle, if you have one.
- **mcp** (profile `tools`) - the MCP server for AI-agent clients.

The full runbook - sizing, exposure, embeddings, BM25, backups and restore,
upgrades - is [docs/compose-deployment.md](../../docs/compose-deployment.md).
The short version:

```bash
# 1. host layout (once)
sudo install -d -o "$USER" /srv/pgokf/{data,bundles,backups}   # or any fast disk
docker network create pgokf-net

# 2. configuration
cp .env.example .env && chmod 600 .env
# fill in the four passwords (openssl rand -hex 24 each), the paths, the images,
# the embedding endpoint/model, and set embedding_dim in PGOKF_POLICY to the
# dimension your model returns.

# 3. start and verify
docker compose up -d
docker compose ps
docker compose exec db psql -U postgres -d okf -c "SELECT jsonb_pretty(pgokf.health());"

# 4. load a bundle (files under /srv/pgokf/bundles/<name>), as the writer role
docker compose exec db psql -U okf_writer -d okf \
  -c "SELECT * FROM pgokf.register_bundle('/bundles/<name>', '<name>');"

# 5. nightly backup (user crontab; no root needed)
#    0 3 * * *  cd /path/to/deploy/compose && docker compose run --rm backup >> backup.log 2>&1
# 6. restore into an empty stack
docker compose run --rm backup pgokf-restore /backups/okf-<stamp>.dump
```

The `embed` service picks up new concepts on its own; run
`SELECT pgokf.rebuild_embedding_index();` as `okf_admin` after the first bulk
load. For BM25 ranking set `search_backend` to `bm25` and run
`SELECT pgokf.rebuild_search_index();`.
