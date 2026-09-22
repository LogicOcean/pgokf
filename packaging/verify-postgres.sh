#!/usr/bin/env bash
# Fail closed on unreviewed major/beta transitions, including installed dev files.
set -euo pipefail
major=${1:?PostgreSQL major required}
image_tag=${2:-$major}
case "$major:$image_tag" in
    15:15|16:16|17:17|18:18) pattern="$major.*" ;;
    19:19beta3) pattern='19~beta3-*' ;;
    *) echo "Unreviewed PostgreSQL major/base transition: $major:$image_tag" >&2; exit 1 ;;
esac
for package in "postgresql-$major" "postgresql-server-dev-$major"; do
    version=$(dpkg-query -W -f='${Version}' "$package")
    # shellcheck disable=SC2053 # Intentional reviewed version pattern.
    if [[ "$version" != $pattern ]]; then
        echo "$package: expected $pattern, got $version" >&2
        exit 1
    fi
done
