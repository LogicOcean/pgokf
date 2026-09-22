#!/usr/bin/env bash
set -euo pipefail
TOOLS=$(cd -- "$(dirname -- "$0")/.." && pwd -P)
PG_CONFIG=${1:?pg_config required}
version=$(python3 "$TOOLS/scripts/compatibility-source.py" "$PWD")
if [[ "$version" == 0.2.0 ]]; then
    python3 "$TOOLS/scripts/test-historical-install.py" "$PWD" "$PG_CONFIG"
else
    docker run --rm --entrypoint cat ghcr.io/logicocean/pgokf:0.2.0-pg18 \
        /usr/share/postgresql/18/extension/pgokf--0.2.0.sql \
        > "$("$PG_CONFIG" --sharedir)/extension/pgokf--0.2.0.sql"
    python3 "$TOOLS/scripts/upgrade-parity.py" --source-root "$PWD" --pg-config "$PG_CONFIG"
fi
