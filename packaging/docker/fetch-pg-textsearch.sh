#!/usr/bin/env bash
#
# fetch-pg-textsearch.sh -- download the pinned Tiger Data pg_textsearch .deb
# for one (PostgreSQL major, Debian architecture) and verify it against
# packaging/docker/pg_textsearch.sha256 before handing it over.
#
# The one place the release layout is known: pg_textsearch publishes a zip per
# (major, arch) on its GitHub release, each holding exactly one .deb. Both the
# image installer and the CI test workflow install the provider through this
# script, so the download, the pinned-name lookup, and the checksum check
# cannot drift apart.
#
# Usage:
#   fetch-pg-textsearch.sh <pg_major> <arch> <dest_dir>   download + verify
#   fetch-pg-textsearch.sh --check <pg_major> <arch>      lookup only
#
# On success the verified .deb path is printed on stdout. Exit codes:
#   0  verified .deb written to <dest_dir> (or, with --check, a pinned entry exists)
#   3  no pinned package for this (major, arch, version): nothing was downloaded
#   1  any other failure (download, extraction, checksum mismatch, bad arguments)
#
# Environment:
#   PG_TEXTSEARCH_VERSION    release to fetch                  (default 1.4.0)
#   PG_TEXTSEARCH_CHECKSUMS  path of the pinned table           (default: next to this script)
#   PG_TEXTSEARCH_BASE_URL   release download root (override for a mirror)
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)"
VERSION="${PG_TEXTSEARCH_VERSION:-1.4.0}"
TABLE="${PG_TEXTSEARCH_CHECKSUMS:-${SCRIPT_DIR}/pg_textsearch.sha256}"
BASE_URL="${PG_TEXTSEARCH_BASE_URL:-https://github.com/timescale/pg_textsearch/releases/download}"

die() { printf 'error: %s\n' "$*" >&2; exit 1; }

check_only=0
if [ "${1:-}" = "--check" ]; then check_only=1; shift; fi
major="${1:-}"; arch="${2:-}"; dest="${3:-}"
if [ -z "${major}" ] || [ -z "${arch}" ]; then
  die "usage: $0 [--check] <pg_major> <arch> [<dest_dir>]"
fi
if [ "${check_only}" = 0 ] && [ -z "${dest}" ]; then
  die "usage: $0 <pg_major> <arch> <dest_dir>"
fi
[ -r "${TABLE}" ] || die "checksum table ${TABLE} is missing or unreadable"

# The pinned file name for this (major, arch, version): exact field match on a
# line carrying a well-formed digest; comment lines can never match (their
# first field is '#'). Dots in the version are literal.
version_re="${VERSION//./[.]}"
pinned="$(awk -v re="^pg-textsearch-postgresql-${major}_${version_re}-[0-9]+_${arch}[.]deb\$" \
  '$1 ~ /^[0-9a-f]{64}$/ && $2 ~ re { print $2; exit }' "${TABLE}")"
if [ -z "${pinned}" ]; then
  printf 'no pinned pg_textsearch %s package for PostgreSQL %s/%s in %s\n' "${VERSION}" "${major}" "${arch}" "${TABLE}" >&2
  exit 3
fi
[ "${check_only}" = 0 ] || exit 0

for tool in curl unzip sha256sum; do
  command -v "${tool}" >/dev/null 2>&1 || die "${tool} not found"
done
mkdir -p -- "${dest}"

work="$(mktemp -d "${TMPDIR:-/tmp}/pg-textsearch-fetch.XXXXXX")"
trap 'rm -rf "${work}"' EXIT

zip="pg-textsearch-v${VERSION}-pg${major}-${arch}.zip"
printf '==> fetching %s\n' "${zip}" >&2
curl -fsSL --retry 3 -o "${work}/${zip}" "${BASE_URL}/v${VERSION}/${zip}"
mkdir "${work}/x"
unzip -q "${work}/${zip}" -d "${work}/x"
[ -f "${work}/x/${pinned}" ] || die "${zip} does not contain the pinned package ${pinned}"

# Verify against the pinned digest before the file leaves the work directory.
(cd "${work}/x" \
  && awk -v f="${pinned}" '$2 == f && $1 ~ /^[0-9a-f]{64}$/ { print; exit }' "${TABLE}" \
   | sha256sum -c - >&2)
mv -- "${work}/x/${pinned}" "${dest}/${pinned}"
printf '%s\n' "${dest}/${pinned}"
