#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# test-build-deb.sh -- maintained tests for packaging/deb/build-deb.sh.
#
# Every test drives the real build script end-to-end inside a disposable
# sandbox fixture: cargo and pg_config are replaced by recording stubs, the
# fixture supplies the pgrx package tree, LICENSE and THIRD-PARTY-NOTICES.txt
# inputs, and dpkg/dpkg-deb are stubbed so the suite runs on any host.
#
# Security contract under test: package metadata overrides are pure data.
# A MAINTAINER or DEB_REVISION carrying newline-bearing shell payloads must
# be rejected before any build step runs, and documented valid overrides
# (including '&', '+' and UTF-8 bytes in the maintainer, '+'/'.'/'~' in the
# revision) must render literally, with no substitution semantics.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/pgokf-deb-tests.XXXXXX")"
trap 'rm -rf -- "${WORK}"' EXIT

pass() { printf 'ok - %s\n' "$1"; }
fail() { printf 'not ok - %s\n' "$1" >&2; exit 1; }

# new_fixture <name>: disposable sandbox whose repo root is the fixture dir.
new_fixture() {
  FIXTURE="${WORK}/$1"
  mkdir -p \
    "${FIXTURE}/packaging/deb" \
    "${FIXTURE}/crates/extension" \
    "${FIXTURE}/stub-bin" \
    "${FIXTURE}/output"
  cp -- \
    "${SCRIPT_DIR}/build-deb.sh" \
    "${SCRIPT_DIR}/control.template" \
    "${FIXTURE}/packaging/deb/"
  printf "default_version = '0.4.0'\n" >"${FIXTURE}/crates/extension/pgokf.control"
  printf 'license fixture\n' >"${FIXTURE}/LICENSE"
  printf 'third-party notices fixture\n' >"${FIXTURE}/THIRD-PARTY-NOTICES.txt"

  # cargo stub: fakes `cargo pgrx package` by materializing the staged tree.
  cat >"${FIXTURE}/stub-bin/cargo" <<'SH'
#!/bin/sh
set -eu
root="$(cd -- ../.. && pwd)"
staged="${root}/target/release/pgokf-pg18/usr"
mkdir -p "${staged}/lib/postgresql/18/lib" "${staged}/share/postgresql/18/extension"
printf 'shared-library fixture\n' >"${staged}/lib/postgresql/18/lib/pgokf.so"
printf 'extension control fixture\n' >"${staged}/share/postgresql/18/extension/pgokf.control"
printf 'extension sql fixture\n' >"${staged}/share/postgresql/18/extension/pgokf--0.4.0.sql"
printf 'ran\n' >"${root}/cargo-ran"
SH
  cat >"${FIXTURE}/stub-bin/dpkg" <<'SH'
#!/bin/sh
set -eu
[ "${1:-}" = '--print-architecture' ]
printf 'amd64\n'
SH
  cat >"${FIXTURE}/stub-bin/dpkg-deb" <<'SH'
#!/bin/sh
set -eu
[ "${1:-}" = '--root-owner-group' ]
[ "${2:-}" = '--build' ]
cp "$3/DEBIAN/control" "${TEST_CAPTURE_CONTROL:?}"
printf 'deb fixture\n' >"$4"
printf '%s\n' "$4" >"${TEST_CAPTURE_DEB_PATH:?}"
SH
  # install stub: accepts exactly the one `install -Dm644` form the build
  # uses (BSD install lacks -D; the stub keeps the suite host-independent).
  cat >"${FIXTURE}/stub-bin/install" <<'SH'
#!/bin/sh
set -eu
[ "${1:-}" = '-Dm644' ]
src="$2"
dst="$3"
mkdir -p "$(dirname -- "$dst")"
cp -- "$src" "$dst"
chmod 0644 "$dst"
SH
  cat >"${FIXTURE}/stub-bin/pg_config" <<'SH'
#!/bin/sh
exit 0
SH
  chmod 0755 "${FIXTURE}/stub-bin/"*
}

# run_build [VAR=value ...]: run the sandboxed build script with a sanitized
# environment; VAR=value assignments are forwarded as environment overrides.
run_build() {
  env \
    -u PG_MAJOR -u PG_CONFIG -u DEB_REVISION -u OUTPUT_DIR -u MAINTAINER \
    PATH="${FIXTURE}/stub-bin:${PATH}" \
    PG_CONFIG="${FIXTURE}/stub-bin/pg_config" \
    OUTPUT_DIR="${FIXTURE}/output" \
    TEST_CAPTURE_CONTROL="${FIXTURE}/rendered-control" \
    TEST_CAPTURE_DEB_PATH="${FIXTURE}/deb-path" \
    "$@" \
    "${FIXTURE}/packaging/deb/build-deb.sh" 18 \
    >"${FIXTURE}/stdout" 2>"${FIXTURE}/stderr"
}

# expect_rejection <test-name> <sentinel-path> <expected-stderr-text> -- env...
expect_rejection() {
  local name="$1" sentinel="$2" expected="$3"
  shift 4
  if run_build "$@"; then
    fail "${name}: build accepted forbidden metadata"
  fi
  [ ! -e "${sentinel}" ] \
    || fail "${name}: metadata payload executed a command (sentinel created)"
  [ ! -e "${FIXTURE}/cargo-ran" ] \
    || fail "${name}: forbidden metadata reached the build step"
  grep -Fq "${expected}" "${FIXTURE}/stderr" \
    || fail "${name}: rejection did not identify the invalid field"
  pass "${name}"
}

test_maintainer_injection_is_rejected() {
  local sentinel payload
  new_fixture maintainer-injection
  sentinel="${FIXTURE}/sentinel"
  # Closes the s-command, injects a sed `e` command (shell execution), and
  # comments out the trailing delimiter: the historic sed-program injection.
  payload="$(printf 'Fixture Maintainer|g\ne touch %s\n#' "${sentinel}")"
  expect_rejection \
    'MAINTAINER sed-program injection is rejected before the build' \
    "${sentinel}" \
    'MAINTAINER must not contain control characters' \
    -- MAINTAINER="${payload}"
}

test_deb_revision_injection_is_rejected() {
  local sentinel payload
  new_fixture deb-revision-injection
  sentinel="${FIXTURE}/sentinel"
  payload="$(printf '1|g\ne touch %s\n#' "${sentinel}")"
  expect_rejection \
    'DEB_REVISION sed-program injection is rejected before the build' \
    "${sentinel}" \
    'DEB_REVISION may contain only' \
    -- DEB_REVISION="${payload}"
}

test_maintainer_carriage_return_is_rejected() {
  new_fixture maintainer-carriage-return
  expect_rejection \
    'MAINTAINER carriage return is rejected' \
    "${FIXTURE}/sentinel" \
    'MAINTAINER must not contain control characters' \
    -- MAINTAINER="$(printf 'Evil\rMaintainer <evil@example.org>')"
}

test_unsafe_deb_revision_characters_are_rejected() {
  local sentinel
  new_fixture unsafe-deb-revision
  sentinel="${FIXTURE}/sentinel"
  # '1$(id)' is deliberately single-quoted: the literal text is the
  # adversarial input, and it must reach the build unexpanded.
  # shellcheck disable=SC2016
  for revision in '1 2' '1-2' "1;touch ${sentinel}" '1$(id)' "$(printf '1\ne true')"; do
    expect_rejection \
      "unsafe DEB_REVISION '${revision}' is rejected" \
      "${sentinel}" \
      'DEB_REVISION may contain only' \
      -- DEB_REVISION="${revision}"
  done
}

test_benign_overrides_render_literally() {
  local maintainer revision expected_deb
  new_fixture benign-overrides
  maintainer='Équipe pgokf & Co <release+deb@example.org>'
  revision='2+dist.1~rc1'
  run_build MAINTAINER="${maintainer}" DEB_REVISION="${revision}" \
    || fail 'documented valid overrides were rejected'
  [ -e "${FIXTURE}/cargo-ran" ] || fail 'valid metadata did not reach the build'
  grep -Fxq "Maintainer: ${maintainer}" "${FIXTURE}/rendered-control" \
    || fail 'valid MAINTAINER was not rendered literally (data lost or & re-interpreted)'
  grep -Fxq "Version: 0.4.0-${revision}" "${FIXTURE}/rendered-control" \
    || fail 'valid DEB_REVISION was not preserved in the package version'
  grep -Fxq 'Package: postgresql-18-pgokf' "${FIXTURE}/rendered-control" \
    || fail 'package name was not rendered'
  grep -Fxq 'Architecture: amd64' "${FIXTURE}/rendered-control" \
    || fail 'architecture was not rendered'
  grep -Fxq 'Depends: postgresql-18, libc6' "${FIXTURE}/rendered-control" \
    || fail 'dependency line was not rendered'
  grep -Eq '^Installed-Size: [0-9]+$' "${FIXTURE}/rendered-control" \
    || fail 'Installed-Size was not rendered as a number'
  if grep -Eq '@[A-Z_]+@' "${FIXTURE}/rendered-control"; then
    fail 'rendered control retained a template token'
  fi
  expected_deb="${FIXTURE}/output/postgresql-18-pgokf_0.4.0-${revision}_amd64.deb"
  [ "$(cat "${FIXTURE}/deb-path")" = "${expected_deb}" ] \
    || fail 'valid DEB_REVISION was not preserved in the package filename'
  [ -f "${expected_deb}" ] || fail 'package builder did not receive the output path'
  pass 'documented valid overrides render literally'
}

test_documented_defaults_render() {
  new_fixture documented-defaults
  run_build || fail 'default metadata was rejected'
  grep -Fxq 'Maintainer: David Saroka <david.saroka@gmail.com>' \
    "${FIXTURE}/rendered-control" \
    || fail 'default MAINTAINER was not preserved'
  grep -Fxq 'Version: 0.4.0-1' "${FIXTURE}/rendered-control" \
    || fail 'default DEB_REVISION was not preserved'
  pass 'documented defaults render unchanged'
}

test_maintainer_value_is_not_resubstituted() {
  local maintainer
  new_fixture maintainer-cascade
  maintainer='Cascade Probe <@ARCH@-@PG_MAJOR@>'
  run_build MAINTAINER="${maintainer}" \
    || fail 'MAINTAINER containing template-like text was rejected'
  grep -Fxq "Maintainer: ${maintainer}" "${FIXTURE}/rendered-control" \
    || fail 'MAINTAINER value was re-interpreted as template tokens'
  grep -Fxq 'Architecture: amd64' "${FIXTURE}/rendered-control" \
    || fail 'architecture rendering was disturbed by the maintainer value'
  pass 'metadata values are data and are never re-substituted'
}

test_undeclared_template_token_fails() {
  new_fixture undeclared-token
  printf 'Undefined: @NOT_A_TOKEN@\n' \
    >>"${FIXTURE}/packaging/deb/control.template"
  if run_build; then
    fail 'undeclared template token was silently rendered'
  fi
  grep -Fq 'undeclared token' "${FIXTURE}/stderr" \
    || fail 'template drift did not name the undeclared token'
  pass 'undeclared control.template tokens fail the build'
}

main() {
  test_maintainer_injection_is_rejected
  test_deb_revision_injection_is_rejected
  test_maintainer_carriage_return_is_rejected
  test_unsafe_deb_revision_characters_are_rejected
  test_benign_overrides_render_literally
  test_documented_defaults_render
  test_maintainer_value_is_not_resubstituted
  test_undeclared_template_token_fails
}

main
printf 'all tests passed\n'
