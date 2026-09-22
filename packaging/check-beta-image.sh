#!/usr/bin/env bash
# Reviewed Docker Official Image index. Tag disappearance or drift is an error.
set -euo pipefail
expected=sha256:f0056b553c58e4533ba81921d9bd5b49641d4fa176e1add956a678b034007ac4
actual=$(docker buildx imagetools inspect postgres:19beta3 --format '{{.Manifest.Digest}}')
[[ "$actual" == "$expected" ]] || {
    echo "postgres:19beta3 changed or disappeared: expected $expected, got $actual" >&2
    exit 1
}
