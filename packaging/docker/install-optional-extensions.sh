#!/usr/bin/env bash
#
# install-optional-extensions.sh -- runtime-stage helper for
# packaging/docker/Dockerfile.
#
# pgokf is compiled with no build-time reference to any other extension, but it
# lights up extra capabilities when these are present on the server:
#
#   pgvector   semantic + hybrid search (concept_search_semantic / _hybrid)
#   pg_cron    in-database scheduled refresh (schedule_refresh)
#   pg_search  ParadeDB BM25 ranking (search_backend = bm25)
#
# pgvector and pg_cron come from the PGDG apt repository the official postgres
# image is already configured with. pg_search is not in PGDG; it is fetched from
# the pinned ParadeDB GitHub release for this image's Debian codename and
# architecture and verified against packaging/docker/pg_search.sha256 before it
# is installed. Each extension is toggled by a WITH_* build argument.
#
# Environment (all supplied by the Dockerfile from build arguments):
#   PG_MAJOR            PostgreSQL major of the base image          (required)
#   WITH_PGVECTOR       1/0 install pgvector                        (default 1)
#   WITH_PG_CRON        1/0 install pg_cron                         (default 1)
#   WITH_PG_SEARCH      1/0 install pg_search                       (default 1)
#   PG_SEARCH_VERSION   ParadeDB release to install                 (default 0.25.6)
#   PG_SEARCH_CHECKSUMS path of the pinned checksum table
#   PG_SEARCH_BASE_URL  release download root (override for a mirror)
set -euo pipefail

PG_MAJOR="${PG_MAJOR:?PG_MAJOR is required}"
WITH_PGVECTOR="${WITH_PGVECTOR:-1}"
WITH_PG_CRON="${WITH_PG_CRON:-1}"
WITH_PG_SEARCH="${WITH_PG_SEARCH:-1}"
PG_SEARCH_VERSION="${PG_SEARCH_VERSION:-0.25.6}"
PG_SEARCH_CHECKSUMS="${PG_SEARCH_CHECKSUMS:-$(dirname -- "${BASH_SOURCE[0]}")/pg_search.sha256}"
PG_SEARCH_BASE_URL="${PG_SEARCH_BASE_URL:-https://github.com/paradedb/paradedb/releases/download}"

log() { printf '==> %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

# Accept the usual spellings of a boolean build argument.
enabled() {
  case "${2,,}" in
    1|true|yes|on) return 0 ;;
    0|false|no|off) return 1 ;;
    *) die "$1 must be 1 or 0 (got '$2')" ;;
  esac
}

packages=()
enabled WITH_PGVECTOR "${WITH_PGVECTOR}" && packages+=("postgresql-${PG_MAJOR}-pgvector")
enabled WITH_PG_CRON "${WITH_PG_CRON}" && packages+=("postgresql-${PG_MAJOR}-cron")

want_pg_search=0
if enabled WITH_PG_SEARCH "${WITH_PG_SEARCH}"; then
  # pg_search's package depends on pgvector; refuse a contradictory build early
  # rather than letting apt report a dependency failure halfway through.
  enabled WITH_PGVECTOR "${WITH_PGVECTOR}" \
    || die "WITH_PG_SEARCH=1 requires WITH_PGVECTOR=1 (pg_search depends on pgvector)"
  want_pg_search=1
  # curl and its trust store are needed only to fetch the release asset; curl is
  # purged again below so the runtime image carries no download tooling.
  packages+=(ca-certificates curl)
fi

if [ ${#packages[@]} -eq 0 ]; then
  log "no optional extensions requested; image ships pgokf only"
  exit 0
fi

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends "${packages[@]}"

if [ "${want_pg_search}" = 1 ]; then
  # shellcheck source=/dev/null
  . /etc/os-release
  arch="$(dpkg --print-architecture)"
  deb="postgresql-${PG_MAJOR}-pg-search_${PG_SEARCH_VERSION}-1PARADEDB-${VERSION_CODENAME}_${arch}.deb"

  # The exact file must have a pinned checksum; an unknown combination is a
  # hard error so a version bump can never silently install unverified bytes.
  # Exact-match the file name as a field (never as a pattern) and require a
  # well-formed digest on the same line.
  expected="$(awk -v file="${deb}" '$2 == file && $1 ~ /^[0-9a-f]{64}$/ { print; exit }' "${PG_SEARCH_CHECKSUMS}")"
  [ -n "${expected}" ] || die "no pinned SHA256 for ${deb} in ${PG_SEARCH_CHECKSUMS}; \
regenerate it with packaging/docker/update-pg-search-checksums.sh ${PG_SEARCH_VERSION}, \
or build with WITH_PG_SEARCH=0"

  work="$(mktemp -d)"
  trap 'rm -rf "${work}"' EXIT

  log "fetching ${deb}"
  curl -fsSL --retry 3 -o "${work}/${deb}" "${PG_SEARCH_BASE_URL}/v${PG_SEARCH_VERSION}/${deb}"
  (cd "${work}" && printf '%s\n' "${expected}" | sha256sum -c -)
  apt-get install -y --no-install-recommends "${work}/${deb}"

  apt-get purge -y --auto-remove curl
fi

rm -rf /var/lib/apt/lists/*

log "optional extensions installed for PostgreSQL ${PG_MAJOR}:"
for control in "/usr/share/postgresql/${PG_MAJOR}/extension/"{vector,pg_cron,pg_search}.control; do
  if [ -f "${control}" ]; then
    printf '    %s\n' "$(basename "${control}" .control)" >&2
  fi
done
