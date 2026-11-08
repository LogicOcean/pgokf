#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Compatibility entry point; all product verification and owned cleanup is Rust.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
IMAGE="${1:?usage: smoke-test.sh IMAGE [VERSION]}"
VERSION="${2:-$(cargo run --quiet --locked --manifest-path "$ROOT/Cargo.toml" -p pgokf-verifier -- compatibility-source "$ROOT")}"
MODE=optional
if [[ "${SMOKE_WITH_OPTIONAL:-1}" == 0 ]]; then MODE=plain; fi
EVIDENCE="${SMOKE_EVIDENCE:-${TMPDIR:-/tmp}/pgokf-smoke-${BASHPID}-${RANDOM}}"
exec cargo run --locked --manifest-path "$ROOT/Cargo.toml" -p pgokf-verifier -- image-smoke "$ROOT" "$IMAGE" "$VERSION" "$MODE" "$EVIDENCE"
