#!/usr/bin/env bash
#
# build-deb.sh -- build a Debian package of the pgokf extension for one
# PostgreSQL major version (15-19).
#
# The package installs:
#   * the shared library  -> $(pg_config --pkglibdir)          (usr/lib/postgresql/N/lib)
#   * the .control + .sql  -> $(pg_config --sharedir)/extension (usr/share/postgresql/N/extension)
#
# and is named  postgresql-N-pgokf  with a hard dependency on  postgresql-N,
# mirroring the naming of PGDG's own per-major extension packages.
#
# Usage:
#   packaging/deb/build-deb.sh [PG_MAJOR]
#
# Environment overrides:
#   PG_MAJOR     PostgreSQL major version           (default: 18, or $1)
#   PG_CONFIG    path to the target pg_config        (default: /usr/lib/postgresql/$PG_MAJOR/bin/pg_config)
#   DEB_REVISION Debian package revision             (default: 1)
#   OUTPUT_DIR   directory to write the .deb into    (default: <repo>/target/packaging/deb)
#   MAINTAINER   Debian Maintainer: field            (default: repo author)
#
# The build is DRY: it delegates the entire filesystem-image construction to
# `cargo pgrx package`, whose output tree already mirrors the target root
# (usr/lib/postgresql/N/lib, usr/share/postgresql/N/extension). Staging is a
# single verbatim copy of that tree -- the one canonical "stage the pgrx
# package output" step reused by every packaging format (see docs/packaging.md).
set -euo pipefail

# --------------------------------------------------------------------------
# Resolve paths and parameters
# --------------------------------------------------------------------------
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." >/dev/null 2>&1 && pwd -P)"
EXTENSION_DIR="${REPO_ROOT}/crates/extension"

PG_MAJOR="${1:-${PG_MAJOR:-18}}"
PG_CONFIG="${PG_CONFIG:-/usr/lib/postgresql/${PG_MAJOR}/bin/pg_config}"
DEB_REVISION="${DEB_REVISION:-1}"
OUTPUT_DIR="${OUTPUT_DIR:-${REPO_ROOT}/target/packaging/deb}"
MAINTAINER="${MAINTAINER:-David Saroka <david.saroka@gmail.com>}"
HOMEPAGE="https://github.com/LogicOcean/pgokf"

log() { printf '==> %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

case "${PG_MAJOR}" in
  15|16|17|18|19) ;;
  *) die "unsupported PG_MAJOR '${PG_MAJOR}' (supported: 15 16 17 18 19)" ;;
esac

command -v dpkg-deb  >/dev/null 2>&1 || die "dpkg-deb not found (install the 'dpkg' package)"
command -v cargo     >/dev/null 2>&1 || die "cargo not found"
[ -x "${PG_CONFIG}" ] || die "pg_config not executable at '${PG_CONFIG}' (set PG_CONFIG or install postgresql-server-dev-${PG_MAJOR})"

# The extension version is the single source of truth for the package version.
EXT_VERSION="$(sed -n "s/^default_version *= *'\\([^']*\\)'.*/\\1/p" "${EXTENSION_DIR}/pgokf.control")"
[ -n "${EXT_VERSION}" ] || die "could not read default_version from ${EXTENSION_DIR}/pgokf.control"

ARCH="$(dpkg --print-architecture)"
DEB_VERSION="${EXT_VERSION}-${DEB_REVISION}"
PKG_NAME="postgresql-${PG_MAJOR}-pgokf"
DEB_FILE="${OUTPUT_DIR}/${PKG_NAME}_${DEB_VERSION}_${ARCH}.deb"

# --------------------------------------------------------------------------
# 1. Build the filesystem image with cargo pgrx package (the shared step)
# --------------------------------------------------------------------------
log "Building pgrx package for PostgreSQL ${PG_MAJOR} (ext version ${EXT_VERSION})"
(
  cd "${EXTENSION_DIR}"
  cargo pgrx package \
    --no-default-features --features "pg${PG_MAJOR}" \
    --pg-config "${PG_CONFIG}"
)

STAGED_TREE="${REPO_ROOT}/target/release/pgokf-pg${PG_MAJOR}"
[ -d "${STAGED_TREE}/usr" ] || die "expected pgrx output tree not found at ${STAGED_TREE}/usr"

# --------------------------------------------------------------------------
# 2. Assemble the .deb build root
# --------------------------------------------------------------------------
BUILD_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/pgokf-deb-pg${PG_MAJOR}.XXXXXX")"
trap 'rm -rf "${BUILD_ROOT}"' EXIT

PKG_ROOT="${BUILD_ROOT}/pkgroot"
mkdir -p "${PKG_ROOT}" "${OUTPUT_DIR}"

log "Staging pgrx image into ${PKG_ROOT}"
cp -a "${STAGED_TREE}/usr" "${PKG_ROOT}/usr"

# Enforce the canonical file mode set expected in a .deb payload.
find "${PKG_ROOT}" -type d -exec chmod 0755 {} +
find "${PKG_ROOT}/usr/lib" -name '*.so' -exec chmod 0644 {} +
find "${PKG_ROOT}/usr/share" -type f -exec chmod 0644 {} +

# --------------------------------------------------------------------------
# 3. Render DEBIAN/control from the template
# --------------------------------------------------------------------------
INSTALLED_SIZE="$(du -k -s "${PKG_ROOT}/usr" | cut -f1)"
SYNOPSIS="Materialized PostgreSQL catalog for Open Knowledge Format bundles"

mkdir -p "${PKG_ROOT}/DEBIAN"
sed \
  -e "s|@PG_MAJOR@|${PG_MAJOR}|g" \
  -e "s|@DEB_VERSION@|${DEB_VERSION}|g" \
  -e "s|@ARCH@|${ARCH}|g" \
  -e "s|@MAINTAINER@|${MAINTAINER}|g" \
  -e "s|@HOMEPAGE@|${HOMEPAGE}|g" \
  -e "s|@INSTALLED_SIZE@|${INSTALLED_SIZE}|g" \
  -e "s|@SYNOPSIS@|${SYNOPSIS}|g" \
  "${SCRIPT_DIR}/control.template" > "${PKG_ROOT}/DEBIAN/control"

log "Rendered DEBIAN/control:"
sed 's/^/    /' "${PKG_ROOT}/DEBIAN/control" >&2

# --------------------------------------------------------------------------
# 4. Build the .deb
# --------------------------------------------------------------------------
log "Building ${DEB_FILE}"
dpkg-deb --root-owner-group --build "${PKG_ROOT}" "${DEB_FILE}"

log "Done: ${DEB_FILE}"
printf '%s\n' "${DEB_FILE}"
