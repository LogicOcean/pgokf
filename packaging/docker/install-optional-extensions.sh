#!/usr/bin/env bash
#
# install-optional-extensions.sh -- runtime-stage helper for
# packaging/docker/Dockerfile.
#
# pgokf is compiled with no build-time reference to any other extension, but it
# lights up extra capabilities when these are present on the server:
#
#   pgvector       semantic + hybrid search (concept_search_semantic / _hybrid)
#   pg_cron        in-database scheduled refresh (schedule_refresh)
#   pg_textsearch  Tiger Data BM25 ranking (search_backend = bm25; PostgreSQL
#                  license; PostgreSQL 17 and 18 only)
#   pg_search      ParadeDB BM25 ranking (search_backend = bm25; AGPL-3.0;
#                  off by default)
#
# pgvector and pg_cron come from the PGDG apt repository the official postgres
# image is already configured with. The two BM25 providers are not in PGDG:
# each is fetched from its pinned GitHub release for this image's PostgreSQL
# major and architecture and verified against the committed checksum table
# (pg_textsearch.sha256 / pg_search.sha256) before it is installed
# (pg_textsearch through fetch-pg-textsearch.sh, shared with CI). Each
# extension is toggled by a WITH_* build argument. pg_textsearch's package
# carries no copyright file, so its PostgreSQL-license notice is installed
# alongside it under /usr/share/doc.
#
# Environment (all supplied by the Dockerfile from build arguments):
#   PG_MAJOR                 PostgreSQL major of the base image       (required)
#   WITH_PGVECTOR            1/0 install pgvector                     (default 1)
#   WITH_PG_CRON             1/0 install pg_cron                      (default 1)
#   WITH_PG_TEXTSEARCH       auto/1/0 install pg_textsearch           (default auto:
#                            install where a release package exists, i.e. 17 and 18)
#   PG_TEXTSEARCH_VERSION    pg_textsearch release to install         (default 1.4.0)
#   WITH_PG_SEARCH           1/0 install pg_search                    (default 0)
#   PG_SEARCH_VERSION        ParadeDB release to install              (default 0.25.6)
#   PG_SEARCH_CHECKSUMS, PG_TEXTSEARCH_CHECKSUMS   paths of the pinned tables
#   PG_SEARCH_BASE_URL, PG_TEXTSEARCH_BASE_URL     release download roots
set -euo pipefail

PG_MAJOR="${PG_MAJOR:?PG_MAJOR is required}"
WITH_PGVECTOR="${WITH_PGVECTOR:-1}"
WITH_PG_CRON="${WITH_PG_CRON:-1}"
WITH_PG_TEXTSEARCH="${WITH_PG_TEXTSEARCH:-auto}"
PG_TEXTSEARCH_VERSION="${PG_TEXTSEARCH_VERSION:-1.4.0}"
PG_TEXTSEARCH_CHECKSUMS="${PG_TEXTSEARCH_CHECKSUMS:-$(dirname -- "${BASH_SOURCE[0]}")/pg_textsearch.sha256}"
PG_TEXTSEARCH_BASE_URL="${PG_TEXTSEARCH_BASE_URL:-https://github.com/timescale/pg_textsearch/releases/download}"
WITH_PG_SEARCH="${WITH_PG_SEARCH:-0}"
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

# Verify a downloaded file against the pinned table (exact file-name field
# match, well-formed digest on the same line) and die when no entry exists,
# so a version bump can never silently install unverified bytes.
verify_pinned() { # file-path table regenerate-hint
  local file="$1" table="$2" hint="$3" name expected
  name="$(basename -- "${file}")"
  expected="$(awk -v f="${name}" '$2 == f && $1 ~ /^[0-9a-f]{64}$/ { print; exit }' "${table}")"
  [ -n "${expected}" ] || die "no pinned SHA256 for ${name} in ${table}; regenerate it with ${hint}"
  (cd "$(dirname -- "${file}")" && printf '%s\n' "${expected}" | sha256sum -c -)
}

packages=()
enabled WITH_PGVECTOR "${WITH_PGVECTOR}" && packages+=("postgresql-${PG_MAJOR}-pgvector")
enabled WITH_PG_CRON "${WITH_PG_CRON}" && packages+=("postgresql-${PG_MAJOR}-cron")

# pg_textsearch: `auto` installs wherever Tiger Data publishes a package (a
# pinned entry exists), `1` requires one, `0` skips. The lookup happens here,
# before anything is downloaded, so a required-but-unpinned combination fails
# with the regenerate hint rather than a download error.
arch="$(dpkg --print-architecture)"
fetch_pg_textsearch="$(dirname -- "${BASH_SOURCE[0]}")/fetch-pg-textsearch.sh"
export PG_TEXTSEARCH_VERSION PG_TEXTSEARCH_CHECKSUMS PG_TEXTSEARCH_BASE_URL
want_pg_textsearch=0
textsearch_mode=skip
case "${WITH_PG_TEXTSEARCH,,}" in
  auto) textsearch_mode=auto ;;
  *) enabled WITH_PG_TEXTSEARCH "${WITH_PG_TEXTSEARCH}" && textsearch_mode=required ;;
esac
if [ "${textsearch_mode}" != skip ]; then
  if "${fetch_pg_textsearch}" --check "${PG_MAJOR}" "${arch}" 2>/dev/null; then
    want_pg_textsearch=1
  elif [ "${textsearch_mode}" = required ]; then
    die "no pinned pg_textsearch ${PG_TEXTSEARCH_VERSION} package for PostgreSQL ${PG_MAJOR}/${arch} in ${PG_TEXTSEARCH_CHECKSUMS}; \
regenerate it with packaging/docker/update-pg-textsearch-checksums.sh ${PG_TEXTSEARCH_VERSION} \
(pg_textsearch ships for PostgreSQL 17 and 18), or build with WITH_PG_TEXTSEARCH=0"
  else
    log "pg_textsearch ${PG_TEXTSEARCH_VERSION} has no package for PostgreSQL ${PG_MAJOR}/${arch}; skipping (WITH_PG_TEXTSEARCH=auto)"
  fi
fi

want_pg_search=0
if enabled WITH_PG_SEARCH "${WITH_PG_SEARCH}"; then
  # pg_search's package depends on pgvector; refuse a contradictory build early
  # rather than letting apt report a dependency failure halfway through.
  enabled WITH_PGVECTOR "${WITH_PGVECTOR}" \
    || die "WITH_PG_SEARCH=1 requires WITH_PGVECTOR=1 (pg_search depends on pgvector)"
  want_pg_search=1
fi
# The two BM25 providers both register an access method named bm25 and cannot
# be created in the same database; shipping both in one image only invites
# that error at CREATE EXTENSION time.
if [ "${want_pg_search}" = 1 ] && [ "${want_pg_textsearch}" = 1 ]; then
  die "WITH_PG_SEARCH=1 needs WITH_PG_TEXTSEARCH=0: the image cannot carry both BM25 providers (both define the bm25 access method; WITH_PG_TEXTSEARCH is '${WITH_PG_TEXTSEARCH}')"
fi

if [ "${want_pg_search}" = 1 ] || [ "${want_pg_textsearch}" = 1 ]; then
  # curl and its trust store are needed only to fetch the release assets; curl
  # (and unzip, for pg_textsearch's zip) are purged again below so the runtime
  # image carries no download tooling.
  packages+=(ca-certificates curl)
  [ "${want_pg_textsearch}" = 1 ] && packages+=(unzip)
fi

if [ ${#packages[@]} -eq 0 ]; then
  log "no optional extensions requested; image ships pgokf only"
  exit 0
fi

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends "${packages[@]}"

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

if [ "${want_pg_textsearch}" = 1 ]; then
  # Download, extraction, and the checksum check live in the shared script;
  # it prints the verified .deb path only after the digest matched.
  deb="$("${fetch_pg_textsearch}" "${PG_MAJOR}" "${arch}" "${work}")"
  apt-get install -y --no-install-recommends "${deb}"
  rm -f "${deb}"
  install -D -m 0644 "$(dirname -- "${BASH_SOURCE[0]}")/pg_textsearch-LICENSE" \
    "/usr/share/doc/pg-textsearch-postgresql-${PG_MAJOR}/copyright"
fi

if [ "${want_pg_search}" = 1 ]; then
  # shellcheck source=/dev/null
  . /etc/os-release
  deb="${work}/postgresql-${PG_MAJOR}-pg-search_${PG_SEARCH_VERSION}-1PARADEDB-${VERSION_CODENAME}_${arch}.deb"
  log "fetching $(basename -- "${deb}")"
  curl -fsSL --retry 3 -o "${deb}" "${PG_SEARCH_BASE_URL}/v${PG_SEARCH_VERSION}/$(basename -- "${deb}")"
  verify_pinned "${deb}" "${PG_SEARCH_CHECKSUMS}" \
    "packaging/docker/update-pg-search-checksums.sh ${PG_SEARCH_VERSION} (or build with WITH_PG_SEARCH=0)"
  apt-get install -y --no-install-recommends "${deb}"
  rm -f "${deb}"
fi

if [ "${want_pg_search}" = 1 ] || [ "${want_pg_textsearch}" = 1 ]; then
  apt-get purge -y --auto-remove curl unzip
fi

rm -rf /var/lib/apt/lists/*

log "optional extensions installed for PostgreSQL ${PG_MAJOR}:"
for control in "/usr/share/postgresql/${PG_MAJOR}/extension/"{vector,pg_cron,pg_textsearch,pg_search}.control; do
  if [ -f "${control}" ]; then
    printf '    %s\n' "$(basename "${control}" .control)" >&2
  fi
done
