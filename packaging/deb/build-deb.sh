#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# build-deb.sh -- build a Debian package of the pgokf extension for one
# PostgreSQL major version (15-18).
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
#
# Package metadata contract: environment-provided metadata (MAINTAINER,
# DEB_REVISION) is validated before any build work and rendered as pure
# data. It must not contain control characters (dpkg fields are
# single-line), DEB_REVISION is restricted to the Debian revision charset
# [A-Za-z0-9.+~], and no value is ever spliced into an executable sed or
# shell program (see render_control below).
set -euo pipefail

# Byte-exact processing for the whole build: control-character detection,
# version parsing, and filename construction must not vary with the host
# locale.
export LC_ALL=C

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
  15|16|17|18) ;;
  *) die "unsupported PG_MAJOR '${PG_MAJOR}' (supported: 15 16 17 18)" ;;
esac

# Metadata validation gates. These run before any tool check or build work
# so hostile metadata can never reach a packaging step. Control characters
# are the shell/sed injection vector (a newline-bearing MAINTAINER once
# became an executable sed `e` command) and dpkg fields are single-line.
# The case pattern matches the whole value, so CR and LF are caught even
# though line-oriented tools (grep, sed) never see the LF separator byte.
die_if_control_chars() {
  case "$2" in
    *[[:cntrl:]]*) die "$1 must not contain control characters (CR, LF, TAB, ESC, ...)" ;;
  esac
}

case "${DEB_REVISION}" in
  *[!A-Za-z0-9.+~]*)
    die "DEB_REVISION may contain only ASCII alphanumerics, '+', '.', '~' (Debian revision charset); got '${DEB_REVISION}'"
    ;;
esac
[ -n "${MAINTAINER}" ] || die "MAINTAINER must not be empty"
die_if_control_chars MAINTAINER "${MAINTAINER}"

command -v dpkg-deb  >/dev/null 2>&1 || die "dpkg-deb not found (install the 'dpkg' package)"
command -v cargo     >/dev/null 2>&1 || die "cargo not found"
[ -x "${PG_CONFIG}" ] || die "pg_config not executable at '${PG_CONFIG}' (set PG_CONFIG or install postgresql-server-dev-${PG_MAJOR})"

# The extension version is the single source of truth for the package version.
EXT_VERSION="$(sed -n "s/^default_version *= *'\\([^']*\\)'.*/\\1/p" "${EXTENSION_DIR}/pgokf.control")"
[ -n "${EXT_VERSION}" ] || die "could not read default_version from ${EXTENSION_DIR}/pgokf.control"
case "${EXT_VERSION}" in
  [-.]*) die "extension version must not begin with '-' or '.': '${EXT_VERSION}'" ;;
  *[!A-Za-z0-9.+~-]*)
    die "extension version contains characters outside the Debian version charset: '${EXT_VERSION}'"
    ;;
esac

ARCH="$(dpkg --print-architecture)"
die_if_control_chars ARCH "${ARCH}"
case "${ARCH}" in
  ''|*[!A-Za-z0-9]*) die "unusable dpkg architecture '${ARCH}'" ;;
esac
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

# Ship the license and the generated third-party notices in the conventional
# Debian doc directory. COMM-LICENSE.md section 5 tells commercial licensees a
# notices file accompanies each release, so this is a licensing obligation, not
# a nicety; the build fails loudly if it has not been generated.
DOC_DIR="${PKG_ROOT}/usr/share/doc/${PKG_NAME}"
if [ ! -f "${REPO_ROOT}/THIRD-PARTY-NOTICES.txt" ]; then
  echo "error: THIRD-PARTY-NOTICES.txt is missing; regenerate it with" >&2
  echo "  cargo about generate --locked --workspace --all-features -o /dev/stdout about.hbs \\" >&2
  echo "    | LC_ALL=C sed 's/[[:blank:]]*$//' > THIRD-PARTY-NOTICES.txt" >&2
  exit 1
fi
install -Dm644 "${REPO_ROOT}/LICENSE"                 "${DOC_DIR}/copyright"
install -Dm644 "${REPO_ROOT}/THIRD-PARTY-NOTICES.txt" "${DOC_DIR}/THIRD-PARTY-NOTICES.txt"

# Enforce the canonical file mode set expected in a .deb payload.
find "${PKG_ROOT}" -type d -exec chmod 0755 {} +
find "${PKG_ROOT}/usr/lib" -name '*.so' -exec chmod 0644 {} +
find "${PKG_ROOT}/usr/share" -type f -exec chmod 0644 {} +

# --------------------------------------------------------------------------
# 3. Render DEBIAN/control from the template
# --------------------------------------------------------------------------
INSTALLED_SIZE="$(du -k -s "${PKG_ROOT}/usr" | cut -f1)"
case "${INSTALLED_SIZE}" in
  ''|*[!0-9]*) die "could not compute a numeric Installed-Size (got '${INSTALLED_SIZE}')" ;;
esac
SYNOPSIS="Materialized PostgreSQL catalog for Open Knowledge Format bundles"

# control_token_value <token>: the closed set of template tokens and the
# validated values they render to. This lookup table is the only bridge
# between template tokens and metadata; a token absent from it is fatal
# (see render_control), so template drift cannot silently pass through.
control_token_value() {
  case "${1}" in
    '@PG_MAJOR@')       printf '%s' "${PG_MAJOR}" ;;
    '@DEB_VERSION@')    printf '%s' "${DEB_VERSION}" ;;
    '@ARCH@')           printf '%s' "${ARCH}" ;;
    '@MAINTAINER@')     printf '%s' "${MAINTAINER}" ;;
    '@HOMEPAGE@')       printf '%s' "${HOMEPAGE}" ;;
    '@INSTALLED_SIZE@') printf '%s' "${INSTALLED_SIZE}" ;;
    '@SYNOPSIS@')       printf '%s' "${SYNOPSIS}" ;;
    *) return 1 ;;
  esac
}

# render_control <template> <output>: fixed-format, data-only rendering.
# Each template line is scanned once for the fixed '@TOKEN@' delimiters and
# every recognized token is replaced by its validated value, which is
# appended as pure data with no substitution, escaping, or command
# semantics. Metadata never becomes part of an executed program here: the
# historic env-interpolated `sed -e "s|...|${MAINTAINER}|g"` construction
# let a newline-bearing MAINTAINER close the substitute command and run an
# arbitrary GNU sed `e` (shell) command with the build's exit status 0.
render_control() {
  local template="$1" output="$2" line out rest token value
  while IFS= read -r line || [[ -n "${line}" ]]; do
    out=''
    while [[ "${line}" == *@* ]]; do
      out+="${line%%@*}"
      rest="${line#*@}"
      if [[ "${rest}" != *@* ]]; then
        # A single unterminated '@' is literal text, not a token.
        out+='@'
        line="${rest}"
        break
      fi
      token="@${rest%%@*}@"
      line="${rest#*@}"
      if value="$(control_token_value "${token}")"; then
        out+="${value}"
      else
        die "control.template contains undeclared token '${token}'"
      fi
    done
    printf '%s\n' "${out}${line}"
  done <"${template}" >"${output}"
}

mkdir -p "${PKG_ROOT}/DEBIAN"
render_control "${SCRIPT_DIR}/control.template" "${PKG_ROOT}/DEBIAN/control"

log "Rendered DEBIAN/control:"
sed 's/^/    /' "${PKG_ROOT}/DEBIAN/control" >&2

# --------------------------------------------------------------------------
# 4. Build the .deb
# --------------------------------------------------------------------------
log "Building ${DEB_FILE}"
dpkg-deb --root-owner-group --build "${PKG_ROOT}" "${DEB_FILE}"

log "Done: ${DEB_FILE}"
printf '%s\n' "${DEB_FILE}"
