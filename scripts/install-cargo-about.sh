#!/usr/bin/env bash
set -euo pipefail
PATH=/usr/bin:/bin
export PATH
unset BASH_ENV ENV CDPATH TAR_OPTIONS GZIP GZIP_OPT

VERSION='0.9.2'
ARCHIVE="cargo-about-${VERSION}-x86_64-unknown-linux-musl.tar.gz"
SHA256='9099a59e820c38a68b9d65f300662a567d56562f9a10f6aa4c7e86c17c2566af'
BINARY_SHA256='b06bd6a8bfd726cffb90e3e0588e3e0b1cfbb582bf6a34f4c1c2692ba8f2e7b8'
URL="https://github.com/EmbarkStudios/cargo-about/releases/download/${VERSION}/${ARCHIVE}"
MEMBER="cargo-about-${VERSION}-x86_64-unknown-linux-musl/cargo-about"

if [[ $# -ne 1 || -z $1 ]]; then
  echo "usage: $0 NEW_DESTINATION" >&2
  exit 2
fi
if [[ $(uname -s) != Linux || $(uname -m) != x86_64 ]]; then
  echo "cargo-about installer supports only Linux x86_64" >&2
  exit 1
fi

destination=$1
parent=$(dirname -- "$destination")
basename=$(basename -- "$destination")
case $parent in
  /*) lexical_parent=$parent ;;
  *) lexical_parent=$PWD/$parent ;;
esac
resolved_parent=$(realpath -m -- "$lexical_parent")
if [[ $lexical_parent != "$resolved_parent" || ! -d $resolved_parent || -L $resolved_parent ]]; then
  echo "cargo-about destination parent and every ancestor must be real directories: $parent" >&2
  exit 1
fi
parent=$resolved_parent
destination=$parent/$basename
if [[ -e $destination || -L $destination ]]; then
  echo "cargo-about destination must not exist: $destination" >&2
  exit 1
fi

work=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/cargo-about.XXXXXX")
stage=$(mktemp "$parent/.cargo-about.XXXXXX")
cleanup() {
  rm -rf -- "$work"
  rm -f -- "$stage"
}
trap cleanup EXIT

curl --fail --silent --show-error --location \
  --proto '=https' --tlsv1.2 \
  --output "$work/$ARCHIVE" "$URL"
printf '%s  %s\n' "$SHA256" "$work/$ARCHIVE" | sha256sum --check --strict -
tar --extract --gzip --file "$work/$ARCHIVE" --directory "$work" -- "$MEMBER"
if [[ ! -f "$work/$MEMBER" || -L "$work/$MEMBER" ]]; then
  echo "cargo-about archive member is not a regular file" >&2
  exit 1
fi
printf '%s  %s\n' "$BINARY_SHA256" "$work/$MEMBER" | sha256sum --check --strict -
install -m 0555 "$work/$MEMBER" "$stage"
ln "$stage" "$destination"
rm -f -- "$stage"
trap - EXIT
rm -rf -- "$work"
