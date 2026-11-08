#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# The current tooling checkout verifies the caller's source checkout.
set -euo pipefail
TOOLING_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
exec cargo run --locked --manifest-path "$TOOLING_ROOT/Cargo.toml" -p pgokf-verifier -- release-resolve .
