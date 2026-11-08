#!/usr/bin/env bash
set -euo pipefail
TOOLS=$(cd -- "$(dirname -- "$0")/.." && pwd -P)
PG_CONFIG=${1:?pg_config required}
exec cargo run --locked --manifest-path "$TOOLS/Cargo.toml" -p pgokf-verifier -- compatibility-parity "$PWD" "$PG_CONFIG"
