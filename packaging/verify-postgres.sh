#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
if [[ -n "${PGOKF_VERIFIER:-}" ]]; then
    exec "$PGOKF_VERIFIER" postgres-check "${1:?PostgreSQL major required}" "${2:-$1}"
fi
exec cargo run --locked --manifest-path "$ROOT/Cargo.toml" -p pgokf-verifier -- postgres-check "${1:?PostgreSQL major required}" "${2:-$1}"
