#!/usr/bin/env bash
# Run as root in CI. PG19 is compatibility-only, never a stable package leg.
set -euo pipefail
major=${1:?PostgreSQL major required}
component=main
image_tag=$major
case "$major" in
    15|16|17|18) ;;
    19) component='main 19'; image_tag=19beta3 ;;
    *) echo "Unreviewed PostgreSQL major: $major" >&2; exit 1 ;;
esac
# shellcheck source=/dev/null
. /etc/os-release
install -d /usr/share/postgresql-common/pgdg
curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc \
    | gpg --batch --yes --dearmor -o /usr/share/postgresql-common/pgdg/apt.postgresql.org.gpg
echo "deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.gpg] https://apt.postgresql.org/pub/repos/apt ${VERSION_CODENAME}-pgdg $component" > /etc/apt/sources.list.d/pgdg.list
apt-get update
packages=()
for package in "postgresql-$major" "postgresql-server-dev-$major"; do
    candidate=$(apt-cache policy "$package" | sed -n 's/^  Candidate: //p')
    if [[ "$major" == 19 && "$candidate" != 19~beta3-* ]]; then
        echo "Pinned 19 Beta 3 missing or changed: $package=$candidate" >&2; exit 1
    fi
    [[ -n "$candidate" && "$candidate" != '(none)' ]] || exit 1
    packages+=("$package=$candidate")
done
apt-get install -y "${packages[@]}" libclang-dev
"$(dirname "$0")/verify-postgres.sh" "$major" "$image_tag"
