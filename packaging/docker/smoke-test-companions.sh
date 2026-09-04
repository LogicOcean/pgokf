#!/usr/bin/env bash
#
# smoke-test-companions.sh -- prove a built pgokf-companions image runs each
# binary, as an unprivileged user, on the daemon's native architecture.
#
# Usage:
#   packaging/docker/smoke-test-companions.sh <image ref>
#
# Environment: DOCKER (default "docker"; e.g. "docker --context <remote>").
set -euo pipefail

IMAGE="${1:?usage: $0 <image ref>}"
DOCKER="${DOCKER:-docker}"

log() { printf '==> %s\n' "$*" >&2; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

for binary in pgokf-ingest pgokf-embed pgokf-mcp; do
  ${DOCKER} run --rm "${IMAGE}" "${binary}" --help >/dev/null \
    || fail "${binary} --help did not run"
  log "ok: ${binary} --help"
done

# The daemons must reject a nonsensical interval up front rather than spin.
if ${DOCKER} run --rm -e OKF_PG_URL=postgresql://x@localhost/x -e OKF_EMBED_ENDPOINT=http://localhost \
     -e OKF_EMBED_MODEL=m "${IMAGE}" pgokf-embed --watch --interval 0 2>/dev/null; then
  fail "pgokf-embed accepted --interval 0"
fi
log "ok: pgokf-embed rejects --interval 0"

uid="$(${DOCKER} run --rm --entrypoint id "${IMAGE}" -u)"
[ "${uid}" = "10001" ] || fail "expected to run as uid 10001, got ${uid}"
log "ok: runs as unprivileged uid ${uid}"

log "PASS: ${IMAGE}"
