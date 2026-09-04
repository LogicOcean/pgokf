#!/usr/bin/env bash
#
# update-pg-search-checksums.sh -- regenerate packaging/docker/pg_search.sha256,
# the pinned SHA256 table that install-optional-extensions.sh verifies the
# ParadeDB pg_search .deb against before installing it into the image.
#
# Usage:
#   packaging/docker/update-pg-search-checksums.sh <pg_search version> [codename ...]
#
# Downloads every (PostgreSQL major, codename, architecture) package for the
# given release from the ParadeDB GitHub release, hashes it, and rewrites the
# table. Run it when bumping PG_SEARCH_VERSION in the Dockerfile, or when the
# official postgres base image moves to a new Debian codename.
#
# Environment overrides:
#   PG_MAJORS     space-separated majors to cover     (default: "15 16 17 18")
#   ARCHES        space-separated Debian architectures (default: "amd64 arm64")
#   BASE_URL      release download root
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)"
OUTPUT="${SCRIPT_DIR}/pg_search.sha256"

VERSION="${1:-}"
[ -n "${VERSION}" ] || { echo "usage: $0 <pg_search version> [codename ...]" >&2; exit 2; }
shift
CODENAMES=("$@")
[ ${#CODENAMES[@]} -gt 0 ] || CODENAMES=(trixie)

PG_MAJORS="${PG_MAJORS:-15 16 17 18}"
ARCHES="${ARCHES:-amd64 arm64}"
BASE_URL="${BASE_URL:-https://github.com/paradedb/paradedb/releases/download}"

command -v curl >/dev/null 2>&1 || { echo "error: curl not found" >&2; exit 1; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/pg-search-sums.XXXXXX")"
trap 'rm -rf "${WORK}"' EXIT

for codename in "${CODENAMES[@]}"; do
  for major in ${PG_MAJORS}; do
    for arch in ${ARCHES}; do
      deb="postgresql-${major}-pg-search_${VERSION}-1PARADEDB-${codename}_${arch}.deb"
      echo "==> ${deb}" >&2
      curl -fsSL --retry 3 -o "${WORK}/${deb}" "${BASE_URL}/v${VERSION}/${deb}"
    done
  done
done

{
  cat <<HEADER
# SHA256 checksums for the ParadeDB pg_search .deb packages that
# packaging/docker/install-optional-extensions.sh installs into the image.
#
# One line per (PostgreSQL major, Debian codename, architecture) in the standard
# \`sha256sum\` format. The installer looks up the exact file it is about to
# install and refuses to proceed when no pinned entry exists, so a bumped
# PG_SEARCH_VERSION or a new base-image codename must be accompanied by a
# regenerated table:
#
#   packaging/docker/update-pg-search-checksums.sh <pg_search version>
#
# Generated for pg_search ${VERSION} (Debian ${CODENAMES[*]}, ${ARCHES// /, }).
HEADER
  (cd "${WORK}" && sha256sum -- *.deb)
} > "${OUTPUT}"

echo "wrote $(grep -c '\.deb$' "${OUTPUT}") entries to ${OUTPUT}" >&2
