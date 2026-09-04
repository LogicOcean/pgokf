#!/usr/bin/env bash
#
# 20-roles.sh -- create the pgokf login roles on first cluster initialization.
#
# pgokf ships three NOLOGIN tiers (pgokf_reader < pgokf_writer < pgokf_admin)
# that are granted to real login roles. This hook creates one login role per
# tier when its password is supplied through the environment, so a deployment
# gets least-privilege accounts without a manual SQL step:
#
#   PGOKF_ADMIN_PASSWORD   -> role ${PGOKF_ADMIN_ROLE:-okf_admin}   (pgokf_admin)
#   PGOKF_WRITER_PASSWORD  -> role ${PGOKF_WRITER_ROLE:-okf_writer} (pgokf_writer)
#   PGOKF_READER_PASSWORD  -> role ${PGOKF_READER_ROLE:-okf_reader} (pgokf_reader)
#
# Each password may instead come from a file (Docker/compose secrets) through
# the matching *_FILE variable, e.g. PGOKF_ADMIN_PASSWORD_FILE=/run/secrets/x.
# A tier whose password is unset or empty is skipped with a notice.
#
# Passwords never appear on a command line or in the server log: psql reads
# them from the environment with \getenv and binds them as quoted literals,
# the role is checked for existence first (a clash is a plain error with no
# password in it), and statement logging is muted for the one statement that
# carries the secret. Runs only when the data directory is empty (the standard
# postgres image initdb rule); a failure leaves a marker that makes the image
# refuse to start until the data directory is wiped, so a half-provisioned
# cluster never comes up silently.
set -euo pipefail
trap 'touch "${PGDATA:?}/.pgokf-init-failed"' ERR

# Resolve VAR or VAR_FILE (the postgres image's own convention), never both.
secret_from_env() {
  local name="$1" file_var="$1_FILE"
  if [ -n "${!name:-}" ] && [ -n "${!file_var:-}" ]; then
    echo "pgokf initdb: both ${name} and ${file_var} are set; use one" >&2
    return 1
  fi
  if [ -n "${!file_var:-}" ]; then
    cat "${!file_var}"
  else
    printf '%s' "${!name:-}"
  fi
}

create_login_role() {
  local role="$1" password="$2" tier="$3"
  if [ -z "${password}" ]; then
    echo "pgokf initdb: skipping ${tier} login role (no password supplied)"
    return 0
  fi
  PGOKF_ROLE_PASSWORD="${password}" psql -v ON_ERROR_STOP=1 \
      --username "${POSTGRES_USER}" --dbname "${POSTGRES_DB}" \
      -v role="${role}" -v tier="${tier}" <<-'SQL'
	SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = :'role') AS role_exists \gset
	\if :role_exists
	    \echo pgokf initdb: role :role already exists; choose another PGOKF_*_ROLE name
	    \quit 1
	\endif
	-- The next statement carries the secret: keep it out of the server log
	-- even if it fails.
	SET log_min_error_statement = panic;
	SET log_statement = none;
	\getenv password PGOKF_ROLE_PASSWORD
	CREATE ROLE :"role" LOGIN PASSWORD :'password';
	RESET log_min_error_statement;
	RESET log_statement;
	GRANT :"tier" TO :"role";
	SQL
  echo "pgokf initdb: created login role ${role} (member of ${tier})"
}

create_login_role "${PGOKF_ADMIN_ROLE:-okf_admin}"   "$(secret_from_env PGOKF_ADMIN_PASSWORD)"  pgokf_admin
create_login_role "${PGOKF_WRITER_ROLE:-okf_writer}" "$(secret_from_env PGOKF_WRITER_PASSWORD)" pgokf_writer
create_login_role "${PGOKF_READER_ROLE:-okf_reader}" "$(secret_from_env PGOKF_READER_PASSWORD)" pgokf_reader
