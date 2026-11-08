#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
exec cargo run --locked --manifest-path "$ROOT/Cargo.toml" -p pgokf-verifier -- package-checksums "${1:?expected pg_textsearch version}" "$ROOT/packaging/docker/pg_textsearch.sha256"
