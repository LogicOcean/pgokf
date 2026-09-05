#!/usr/bin/env bash
#
# smoke-test.sh -- prove a built pgokf server image actually works, end to end.
#
# Starts the image the way the shipped compose stack does (every optional
# extension preloaded, env-driven roles and policy), then asserts:
#   * the pgokf extension is created at the expected version
#   * pgvector, pg_cron, and the BM25 provider (pg_textsearch on 17/18) were
#     created by the first-init hook
#   * the three env-driven login roles exist with the right tier membership
#   * the PGOKF_POLICY JSON was applied (embedding_dim / allowed_roots)
#   * the sample bundle registers and materializes concept rows
#   * the pgokf-backup tool produces a restorable archive
#
# Usage:
#   packaging/docker/smoke-test.sh <image ref> [expected extension version]
#
# The expected version defaults to default_version in pgokf.control. Works
# against any Docker context (the bundle is copied in with `docker cp`, so no
# daemon-side path is needed). Environment: DOCKER (default "docker"; set e.g.
# "docker --context <remote>" to target a remote daemon), SMOKE_WITH_OPTIONAL
# (default 1; set 0 for an image built with the WITH_* extensions off).
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." >/dev/null 2>&1 && pwd -P)"

IMAGE="${1:?usage: $0 <image ref> [expected version]}"
EXPECTED="${2:-$(sed -n "s/^default_version *= *'\([^']*\)'.*/\1/p" "${REPO_ROOT}/crates/extension/pgokf.control")}"
DOCKER="${DOCKER:-docker}"
WITH_OPTIONAL="${SMOKE_WITH_OPTIONAL:-1}"
NAME="pgokf-smoke-$$"

log() { printf '==> %s\n' "$*" >&2; }
fail() { printf 'FAIL: %s\n' "$*" >&2; ${DOCKER} logs "${NAME}" 2>&1 | tail -40 >&2 || true; exit 1; }
cleanup() { ${DOCKER} rm -f "${NAME}" >/dev/null 2>&1 || true; }
trap cleanup EXIT

# psql inside the container as the bootstrap superuser, over TCP so the
# entrypoint's socket-only bootstrap server (which runs the init hooks and is
# then stopped) can never satisfy a probe meant for the real server; -tA for
# bare values.
sql() { ${DOCKER} exec "${NAME}" psql -h 127.0.0.1 -U postgres -d postgres -v ON_ERROR_STOP=1 -tA -c "$1"; }
assert_eq() { # actual expected description
  [ "$1" = "$2" ] || fail "$3: expected '$2', got '$1'"
  log "ok: $3"
}

# Preload whatever optional libraries the image actually carries; an image built
# with a WITH_* off (or the pg19 advisory leg) must still start.
preload="pgokf"
# shellcheck disable=SC2016  # ${PG_MAJOR} is expanded by the container's shell, on purpose
carried="$(${DOCKER} run --rm --entrypoint sh "${IMAGE}" \
  -c 'cd "/usr/share/postgresql/${PG_MAJOR}/extension" && ls pg_cron.control pg_search.control pg_textsearch.control 2>/dev/null' || true)"
case "${carried}" in *pg_cron.control*) preload="${preload},pg_cron" ;; esac
case "${carried}" in *pg_textsearch.control*) preload="${preload},pg_textsearch" ;; esac
case "${carried}" in *pg_search.control*) preload="${preload},pg_search" ;; esac

log "starting ${IMAGE} (shared_preload_libraries=${preload})"
${DOCKER} run -d --name "${NAME}" \
  -e POSTGRES_PASSWORD=smoke -e POSTGRES_HOST_AUTH_METHOD=trust \
  -e PGOKF_ADMIN_PASSWORD=admin-pw \
  -e PGOKF_WRITER_PASSWORD=writer-pw \
  -e PGOKF_READER_PASSWORD=reader-pw \
  -e PGOKF_POLICY='{"embedding_dim": 1024, "allowed_roots": ["/bundles"], "store_source": true}' \
  "${IMAGE}" \
  postgres -c "shared_preload_libraries=${preload}" -c cron.database_name=postgres \
  >/dev/null

# Wait for the real condition (extension created at the expected version), not
# merely for the socket: the entrypoint runs the init hooks after the
# bootstrap server comes up, and polling pg_isready alone races them.
log "waiting for pgokf ${EXPECTED}"
ready=""
for _ in $(seq 1 60); do
  got="$(sql "SELECT extversion FROM pg_extension WHERE extname = 'pgokf'" 2>/dev/null | tr -d '[:space:]' || true)"
  if [ "${got}" = "${EXPECTED}" ]; then ready=1; break; fi
  sleep 2
done
[ -n "${ready}" ] || fail "pgokf extension not present at ${EXPECTED} after 120s"
log "ok: pgokf ${EXPECTED} created"

# Init hooks may still be running for a moment after the extension appears;
# wait for the policy hook, which runs last, before asserting anything else.
applied=""
for _ in $(seq 1 30); do
  dim="$(sql "SELECT pgokf.get_config() ->> 'embedding_dim'" 2>/dev/null | tr -d '[:space:]' || true)"
  if [ "${dim}" = "1024" ]; then applied=1; break; fi
  sleep 2
done
[ -n "${applied}" ] || fail "PGOKF_POLICY was not applied (embedding_dim still '${dim}')"
log "ok: PGOKF_POLICY applied (embedding_dim=1024)"
assert_eq "$(sql "SELECT pgokf.get_config() -> 'allowed_roots'")" '["/bundles"]' "allowed_roots policy"

# Every optional extension the image carries must have been created by the init
# hook (an image built with some WITH_* off simply carries fewer).
available="$(sql "SELECT string_agg(name, ',' ORDER BY name) FROM pg_available_extensions WHERE name IN ('pg_cron','pg_search','pg_textsearch','vector')")"
assert_eq "$(sql "SELECT string_agg(extname, ',' ORDER BY extname) FROM pg_extension WHERE extname IN ('pg_cron','pg_search','pg_textsearch','vector')")" \
  "${available}" "optional extensions created by the init hook (${available:-none})"
if [ "${WITH_OPTIONAL}" = 1 ]; then
  case "${available}" in
    *vector*) log "ok: image carries pgvector" ;;
    *) fail "image does not carry pgvector" ;;
  esac
  case "${available}" in
    *pg_cron*) log "ok: image carries pg_cron" ;;
    *) fail "image does not carry pg_cron" ;;
  esac
  # A BM25 provider ships for 17 and 18; an image of those majors without one
  # is a broken build (stale checksum table, skipped download), not a variant.
  server_major="$(sql "SELECT pg_catalog.current_setting('server_version_num')::int / 10000")"
  case "${server_major}" in
    17|18)
      case ",${available}," in
        *,pg_textsearch,*|*,pg_search,*) log "ok: image carries a BM25 provider" ;;
        *) fail "PostgreSQL ${server_major} image carries no BM25 provider (pg_textsearch ships for 17 and 18)" ;;
      esac
      ;;
  esac
fi

assert_eq "$(sql "SELECT string_agg(r.rolname || ':' || t.rolname, ',' ORDER BY r.rolname)
                    FROM pg_auth_members m
                    JOIN pg_roles r ON r.oid = m.member
                    JOIN pg_roles t ON t.oid = m.roleid
                   WHERE r.rolname IN ('okf_admin','okf_writer','okf_reader') AND r.rolcanlogin")" \
  "okf_admin:pgokf_admin,okf_reader:pgokf_reader,okf_writer:pgokf_writer" "env-driven login roles"

# Register the sample bundle from a path inside the container (no daemon-side
# mount needed). Call register_bundle in FROM, not as (f()).*: the composite-
# star form re-evaluates the volatile function once per column.
${DOCKER} exec "${NAME}" mkdir -p /bundles
${DOCKER} cp "${REPO_ROOT}/examples/sample-bundle" "${NAME}:/bundles/sample"
sql "SELECT * FROM pgokf.register_bundle('/bundles/sample', 'sample')" >/dev/null
concepts="$(sql "SELECT count(*) FROM pgokf.concepts")"
[ "${concepts}" -gt 0 ] || fail "no concept rows materialized from the sample bundle"
log "ok: ${concepts} concept row(s) materialized"

# Scheduled refresh (pg_cron is created in this database, cron.database_name):
# the job command must pin the bundle's tenant before the refresh call, so
# the cron worker's own session satisfies the tenant rules.
case ",${available}," in
  *,pg_cron,*)
    bundle_id="$(sql "SELECT min(id) FROM pgokf.bundles")"
    sql "SELECT pgokf.schedule_refresh(${bundle_id}, '0 3 * * *')" >/dev/null
    assert_eq "$(sql "SELECT command FROM cron.job WHERE jobname = 'pgokf_refresh_${bundle_id}'")" \
      "SELECT pg_catalog.set_config('pgokf.tenant', 'default', false); SELECT pgokf.refresh_bundle(${bundle_id})" \
      "scheduled refresh pins the bundle's tenant"
    assert_eq "$(sql "SELECT pgokf.unschedule_refresh(${bundle_id})")" "t" "scheduled refresh removed"
    ;;
esac
assert_eq "$(sql "SELECT pgokf.health() ->> 'ok'")" "true" "pgokf.health() reports ok"

# BM25 must work for a non-superuser reader (the production case) when the
# image carries a provider: switch the backend, build the index, search as the
# env-created reader role. Superusers bypass row-level security, so only a
# non-owner session proves the provider path.
case ",${available}," in
  *,pg_search,*|*,pg_textsearch,*)
    sql "SELECT pgokf.set_config('search_backend', '\"bm25\"'::jsonb)" >/dev/null
    assert_eq "$(sql "SELECT pgokf.rebuild_search_index()")" "t" "bm25 index built"
    log "ok: bm25 provider resolved to $(sql "SELECT pgokf.search_index_status() -> 'bm25' ->> 'provider'")"
    bm25_hit="$(${DOCKER} exec "${NAME}" psql -h 127.0.0.1 -U okf_reader -d postgres -v ON_ERROR_STOP=1 -tA \
      -c "SELECT concept_id FROM pgokf.concept_search('failover', NULL, 1)")" \
      || fail "bm25 concept_search failed for the non-superuser reader"
    [ -n "${bm25_hit}" ] || fail "bm25 concept_search returned no hit for the reader"
    log "ok: bm25 search as okf_reader -> ${bm25_hit}"
    sql "SELECT pgokf.set_config('search_backend', '\"native\"'::jsonb)" >/dev/null
    ;;
esac
assert_eq "$(sql "SELECT (pgokf.search_index_status() -> 'embedding' ->> 'pgvector_available')")" \
  "$([ "${WITH_OPTIONAL}" = 1 ] && echo true || echo false)" "search_index_status pgvector visibility"

# The backup tool must produce a verifiable archive with the image's own tools.
# Change the policy first so the restore below can prove the DUMPED policy
# wins over the value the restore target's own init hook applies.
sql "SELECT pgokf.set_config('embedding_dim', '768'::jsonb)" >/dev/null
${DOCKER} exec -e PGUSER=postgres -e PGDATABASE=postgres -e PGOKF_BACKUP_DIR=/tmp/smoke-backups \
  "${NAME}" pgokf-backup >/dev/null 2>&1 || fail "pgokf-backup failed"
dumps="$(${DOCKER} exec "${NAME}" sh -c 'ls /tmp/smoke-backups/postgres-*.dump | wc -l')"
assert_eq "${dumps}" "1" "pgokf-backup wrote one archive"
# The archive must carry the catalog DATA, not just the CREATE EXTENSION
# statement: pgokf registers its tables with pg_extension_config_dump, so a
# restore rehydrates bundles and concepts rather than an empty catalog.
${DOCKER} exec "${NAME}" sh -c 'pg_restore --list /tmp/smoke-backups/postgres-*.dump | grep -q "TABLE DATA pgokf concepts "' \
  || fail "backup archive does not contain pgokf.concepts data (pg_extension_config_dump missing?)"
log "ok: backup archive carries the pgokf.concepts data"

# Restore the archive into a SECOND, freshly initialized container of the same
# image with the image's pgokf-restore - the documented disaster-recovery path -
# and prove the catalog comes back: same concept count, the dumped policy (768,
# not the 1024 the target's init hook applied), and a working health probe.
# pgokf-restore runs pg_restore with --exit-on-error, so nothing is "ignored".
RESTORE="${NAME}-restore"
restore_cleanup() { ${DOCKER} rm -f "${RESTORE}" >/dev/null 2>&1 || true; cleanup; }
trap restore_cleanup EXIT
${DOCKER} run -d --name "${RESTORE}" \
  -e POSTGRES_PASSWORD=smoke -e POSTGRES_HOST_AUTH_METHOD=trust \
  -e PGOKF_ADMIN_PASSWORD=admin-pw -e PGOKF_WRITER_PASSWORD=writer-pw -e PGOKF_READER_PASSWORD=reader-pw \
  -e PGOKF_POLICY='{"embedding_dim": 1024, "allowed_roots": ["/bundles"], "store_source": true}' \
  "${IMAGE}" \
  postgres -c "shared_preload_libraries=${preload}" -c cron.database_name=postgres \
  >/dev/null
restored_ready=""
for _ in $(seq 1 60); do
  got="$(${DOCKER} exec "${RESTORE}" psql -h 127.0.0.1 -U postgres -d postgres -tAc \
    "SELECT pgokf.get_config() ->> 'embedding_dim'" 2>/dev/null | tr -d '[:space:]' || true)"
  if [ "${got}" = "1024" ]; then restored_ready=1; break; fi
  sleep 2
done
[ -n "${restored_ready}" ] || fail "restore target did not initialize"
${DOCKER} exec "${NAME}" sh -c 'cat /tmp/smoke-backups/postgres-*.dump' \
  | ${DOCKER} exec -i "${RESTORE}" sh -c 'cat > /tmp/restore.dump'
${DOCKER} exec -e PGUSER=postgres -e PGDATABASE=postgres "${RESTORE}" pgokf-restore /tmp/restore.dump \
  || fail "pgokf-restore into a fresh container reported errors"
restored_concepts="$(${DOCKER} exec "${RESTORE}" psql -h 127.0.0.1 -U postgres -d postgres -tAc 'SELECT count(*) FROM pgokf.concepts')"
assert_eq "${restored_concepts}" "${concepts}" "restore rehydrated the concept rows"
assert_eq "$(${DOCKER} exec "${RESTORE}" psql -h 127.0.0.1 -U postgres -d postgres -tAc "SELECT pgokf.get_config() ->> 'embedding_dim'")" \
  "768" "restore carried the dumped policy over the target's init policy"
assert_eq "$(${DOCKER} exec "${RESTORE}" psql -h 127.0.0.1 -U postgres -d postgres -tAc "SELECT pgokf.health() ->> 'ok'")" \
  "true" "restored catalog is healthy"

log "PASS: ${IMAGE} (pgokf ${EXPECTED})"
