#!/usr/bin/env bash
# Run as root in CI. Every tested major, PG19 included, is a GA release from
# PGDG's main component; PG19 is tested but not yet a published package leg.
set -euo pipefail
major=${1:?PostgreSQL major required}
image_tag=$major
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
verifier() {
    if [[ -n "${PGOKF_VERIFIER:-}" ]]; then "$PGOKF_VERIFIER" "$@";
    else cargo run --locked --manifest-path "$ROOT/Cargo.toml" -p pgokf-verifier -- "$@"; fi
}
verifier postgres-target "$major" "$image_tag"
# shellcheck source=/dev/null
. /etc/os-release
install -d /usr/share/postgresql-common/pgdg
curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc \
    | gpg --batch --yes --dearmor -o /usr/share/postgresql-common/pgdg/apt.postgresql.org.gpg
echo "deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.gpg] https://apt.postgresql.org/pub/repos/apt ${VERSION_CODENAME}-pgdg main" > /etc/apt/sources.list.d/pgdg.list
apt-get update
packages=()
for package in "postgresql-$major" "postgresql-server-dev-$major"; do
    candidate=$(apt-cache policy "$package" | sed -n 's/^  Candidate: //p')
    verifier postgres-candidate "$major" "$image_tag" "$candidate"
    packages+=("$package=$candidate")
done
apt-get install -y "${packages[@]}" libclang-dev
"$(dirname "$0")/verify-postgres.sh" "$major" "$image_tag"
