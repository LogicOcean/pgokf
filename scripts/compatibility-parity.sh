#!/usr/bin/env bash
set -euo pipefail
TOOLS=$(cd -- "$(dirname -- "$0")/.." && pwd -P)
PG_CONFIG=${1:?pg_config required}
python3 "$TOOLS/scripts/compatibility-source.py" "$PWD"
evidence=$(mktemp -d "${TMPDIR:-/tmp}/pgokf-parity-image.XXXXXXXX")
run_id=$(python3 -c 'import uuid; print(uuid.uuid4().hex)')
image=ghcr.io/logicocean/pgokf:0.2.0-pg18
docker pull "$image" >/dev/null
cid=$(python3 "$TOOLS/scripts/create-owned-container.py" pgokf.parity-run "$run_id" \
    "$evidence/create.json" "$image" --entrypoint cat -- \
    /usr/share/postgresql/18/extension/pgokf--0.2.0.sql)
cleanup_image() {
    python3 "$TOOLS/scripts/cleanup-owned-container.py" "$cid" pgokf.parity-run "$run_id" \
        "$evidence/delete.json" "$evidence/create.json"
}
trap cleanup_image EXIT
docker start -a "$cid" > "$("$PG_CONFIG" --sharedir)/extension/pgokf--0.2.0.sql"
python3 "$TOOLS/scripts/upgrade-parity.py" --source-root "$PWD" --pg-config "$PG_CONFIG"
