#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
if [[ -n "${PGOKF_VERIFIER:-}" ]]; then
    exec "$PGOKF_VERIFIER" postgres-beta-image
fi
exec cargo run --locked --manifest-path "$ROOT/Cargo.toml" -p pgokf-verifier -- postgres-beta-image
