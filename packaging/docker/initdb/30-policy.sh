#!/usr/bin/env bash
#
# 30-policy.sh -- apply the durable pgokf catalog policy on first initialization.
#
# PGOKF_POLICY is a JSON object of `pgokf.set_config` keys, for example:
#
#   PGOKF_POLICY='{"embedding_dim": 1024, "store_source": true,
#                  "allowed_roots": ["/bundles"], "search_backend": "native"}'
#
# Every key/value pair is applied through the public `pgokf.set_config(key,
# value)` function, which validates the key and the value's type; an unknown
# key or a malformed value aborts initialization loudly rather than leaving a
# half-configured catalog (and leaves the marker that stops the image from
# starting until the data directory is wiped). Later changes are made with the
# same function from any pgokf_admin session (see docs/configuration.md).
# Unset = defaults.
set -euo pipefail
trap 'touch "${PGDATA:?}/.pgokf-init-failed"' ERR

if [ -z "${PGOKF_POLICY:-}" ]; then
  echo "pgokf initdb: PGOKF_POLICY not set; catalog policy left at defaults"
else
  psql -v ON_ERROR_STOP=1 --username "${POSTGRES_USER}" --dbname "${POSTGRES_DB}" <<-'SQL'
	\getenv policy PGOKF_POLICY
	SELECT pgokf.set_config(key, value)
	  FROM jsonb_each(:'policy'::jsonb)
	 ORDER BY key;
	SELECT jsonb_pretty(pgokf.get_config()) AS effective_policy;
	SQL
  echo "pgokf initdb: catalog policy applied from PGOKF_POLICY"
fi
