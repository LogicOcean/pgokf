#!/usr/bin/env bash
#
# update-pg-textsearch-checksums.sh -- regenerate packaging/docker/pg_textsearch.sha256,
# the pinned SHA256 table that install-optional-extensions.sh verifies the
# Tiger Data pg_textsearch .deb against before installing it into the image.
#
# Usage:
#   packaging/docker/update-pg-textsearch-checksums.sh <pg_textsearch version>
#
# Downloads every (PostgreSQL major, architecture) release zip, extracts the
# .deb it carries, hashes it, and rewrites the table. Run it when bumping
# PG_TEXTSEARCH_VERSION in the Dockerfile.
#
# Environment overrides:
#   PG_MAJORS  space-separated majors to cover     (default: "17 18")
#   ARCHES     space-separated Debian architectures (default: "amd64 arm64")
#   BASE_URL   release download root
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)"
OUTPUT="${SCRIPT_DIR}/pg_textsearch.sha256"

VERSION="${1:-}"
[ -n "${VERSION}" ] || { echo "usage: $0 <pg_textsearch version>" >&2; exit 2; }

PG_MAJORS="${PG_MAJORS:-17 18}"
ARCHES="${ARCHES:-amd64 arm64}"
BASE_URL="${BASE_URL:-https://github.com/timescale/pg_textsearch/releases/download}"

for tool in curl python3; do
  command -v "${tool}" >/dev/null 2>&1 || { echo "error: ${tool} not found" >&2; exit 1; }
done

WORK="$(mktemp -d "${TMPDIR:-/tmp}/pg-textsearch-sums.XXXXXX")"
trap 'rm -rf "${WORK}"' EXIT

for major in ${PG_MAJORS}; do
  for arch in ${ARCHES}; do
    zip="pg-textsearch-v${VERSION}-pg${major}-${arch}.zip"
    echo "==> ${zip}" >&2
    curl -fsSL --retry 3 -o "${WORK}/${zip}" "${BASE_URL}/v${VERSION}/${zip}"
    python3 - "${WORK}/${zip}" "${WORK}" <<'PY'
import sys, zipfile
with zipfile.ZipFile(sys.argv[1]) as archive:
    debs = [name for name in archive.namelist() if name.endswith(".deb")]
    if len(debs) != 1:
        raise SystemExit(f"expected exactly one .deb in {sys.argv[1]}, found {debs}")
    archive.extract(debs[0], sys.argv[2])
PY
    rm -f "${WORK}/${zip}"
  done
done

{
  cat <<HEADER
# SHA256 checksums for the Tiger Data pg_textsearch release packages that
# packaging/docker/install-optional-extensions.sh installs into the image.
#
# pg_textsearch publishes one .deb per (PostgreSQL major, architecture) inside
# a zip archive on its GitHub release (PostgreSQL 17 and 18 only). The
# installer downloads the zip, extracts the .deb, and refuses to install it
# unless its SHA256 matches the entry here. Regenerate with:
#
#   packaging/docker/update-pg-textsearch-checksums.sh <pg_textsearch version>
#
# Generated for pg_textsearch ${VERSION} (${ARCHES// /, }).
HEADER
  (cd "${WORK}" && sha256sum -- *.deb)
} > "${OUTPUT}"

echo "wrote $(grep -c '\.deb$' "${OUTPUT}") entries to ${OUTPUT}" >&2
