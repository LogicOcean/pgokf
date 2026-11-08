#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
EVIDENCE="${SMOKE_EVIDENCE:-${TMPDIR:-/tmp}/pgokf-companions-${BASHPID}-${RANDOM}}"
exec cargo run --locked --manifest-path "$ROOT/Cargo.toml" -p pgokf-verifier -- companions-smoke "${1:?expected image}" "$EVIDENCE"
