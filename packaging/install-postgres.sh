#!/usr/bin/env bash
# Run as root in CI. PG19 is compatibility-only, never a stable package leg.
set -euo pipefail
major=${1:?PostgreSQL major required}
component=main
image_tag=$major
if [[ "$major" == 19 ]]; then component='main 19'; image_tag=19rc1; fi
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
echo "deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.gpg] https://apt.postgresql.org/pub/repos/apt ${VERSION_CODENAME}-pgdg $component" > /etc/apt/sources.list.d/pgdg.list
apt-get update
packages=()
for package in "postgresql-$major" "postgresql-server-dev-$major"; do
    candidate=$(apt-cache policy "$package" | sed -n 's/^  Candidate: //p')
    verifier postgres-candidate "$major" "$image_tag" "$candidate"
    packages+=("$package=$candidate")
done
apt-get install -y "${packages[@]}" libclang-dev
"$(dirname "$0")/verify-postgres.sh" "$major" "$image_tag"
