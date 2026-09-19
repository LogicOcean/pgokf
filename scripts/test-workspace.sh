#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Canonical local PG18 host tests, including extension/API and web crates.
set -euo pipefail
cd "$(dirname "$0")/.."
export LC_ALL=C
if [[ "$(uname -s)" == Darwin ]]; then
  # PostgreSQL supplies these symbols when loading the extension. Darwin's
  # linker otherwise rejects the extension cdylib during host cargo tests.
  export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C link-arg=-Wl,-undefined,dynamic_lookup"
fi
exec cargo test --workspace --no-default-features --features pg18 --locked "$@"
