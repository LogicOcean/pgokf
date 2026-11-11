#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Start a disposable local PostgreSQL cluster; never connect to an existing database.
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
test_pg_config=${PGOKF_TEST_PG_CONFIG:-pg_config}
test_pg_bin=$("$test_pg_config" --bindir)
test_root=$(mktemp -d /tmp/pgokf-confluence-test.XXXXXX)
cleanup() {
    "$test_pg_bin/pg_ctl" -D "$test_root/data" -m immediate -w stop >/dev/null 2>&1 || true
    rm -rf -- "$test_root"
}
trap cleanup EXIT
mkdir "$test_root/socket"
"$test_pg_bin/initdb" -D "$test_root/data" -U postgres -A trust --no-locale --encoding=UTF8 > "$test_root/initdb.log"
# The socket directory is private and TCP listening is disabled. The port only
# names a socket in this unique directory, so no existing server can be reached.
cat >> "$test_root/data/postgresql.conf" <<EOF
listen_addresses = ''
unix_socket_directories = '$test_root/socket'
port = 55432
EOF
# PostgreSQL 18 can load a separately packaged extension without overwriting a
# system installation. Supply the share directory that CONTAINS extension/.
# Escape single quotes for the PostgreSQL config value.
if [[ -n ${PGOKF_TEST_EXTENSION_CONTROL_PATH:-} ]]; then
    control_path=${PGOKF_TEST_EXTENSION_CONTROL_PATH//\'/\'\'}
    printf "extension_control_path = '%s'\n" "$control_path" >> "$test_root/data/postgresql.conf"
fi
# The matching library directory of such a package (for example the
# lib/postgresql/18/lib directory of `cargo pgrx package` output).
if [[ -n ${PGOKF_TEST_EXTENSION_LIBRARY_PATH:-} ]]; then
    library_path=${PGOKF_TEST_EXTENSION_LIBRARY_PATH//\'/\'\'}
    printf "dynamic_library_path = '%s:\$libdir'\n" "$library_path" >> "$test_root/data/postgresql.conf"
fi
"$test_pg_bin/pg_ctl" -D "$test_root/data" -l "$test_root/server.log" -w start > /dev/null
"$test_pg_bin/psql" -X -v ON_ERROR_STOP=1 -h "$test_root/socket" -p 55432 -U postgres -d postgres <<'SQL'
CREATE EXTENSION pgokf;
CREATE ROLE confluence_test_writer LOGIN;
GRANT pgokf_writer TO confluence_test_writer;
-- The loaded shared library must be the build whose SQL was installed: a stale
-- library elsewhere on the library path would test the wrong code.
DO $check$
DECLARE
    installed text := (SELECT extversion FROM pg_extension WHERE extname = 'pgokf');
BEGIN
    IF pgokf.version() IS DISTINCT FROM installed THEN
        RAISE EXCEPTION 'loaded pgokf library % does not match installed extension %',
            pgokf.version(), installed;
    END IF;
END
$check$;
SELECT extversion, pgokf.version() AS library FROM pg_extension WHERE extname = 'pgokf';
SQL
cd "$repo_root"
PGOKF_CONFLUENCE_TEST_DATABASE_URL="host=$test_root/socket port=55432 user=confluence_test_writer dbname=postgres" \
    cargo test --locked -p pgokf-confluence --test catalog -- --ignored --test-threads=1
