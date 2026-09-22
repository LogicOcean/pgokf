#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Resolve the one version every package artifact carries, and whether this
# packages workflow run publishes release images. Publication is
# immutable-tag-only:
#
#   * a version-tag push event publishes, and only when the tag name is
#     exactly v<default_version> from pgokf.control AND the checked-out HEAD
#     is the commit that tag object points at;
#   * a manual workflow_dispatch publishes only when its explicit
#     release_tag input names an existing v<default_version> tag whose commit
#     is the checked-out HEAD (historical catch-up on the immutable tag);
#   * every other ref/input combination builds and smoke-tests but publishes
#     nothing - a branch HEAD or untagged commit can never publish versioned
#     images or manifests.
#
# Environment: GITHUB_REF, GITHUB_OUTPUT (workflow), RELEASE_TAG_INPUT (the
# workflow_dispatch input, empty when unset). Requires a fetch-depth: 0
# checkout so the tag object is present locally.
set -euo pipefail

version=$(sed -n "s/^default_version *= *'\([^']*\)'.*/\1/p" crates/extension/pgokf.control)
[ -n "$version" ] || { echo "::error::could not read default_version from pgokf.control"; exit 1; }
echo "version=$version" >> "$GITHUB_OUTPUT"
echo "resolved extension version: $version"

# Check source identity even for historical catch-up, without depending on
# scripts existing in that historical tree.
python3 - "$version" <<'IDENTITY'
import json, pathlib, sys, tomllib
root = pathlib.Path('.')
version = sys.argv[1]
workspace = tomllib.loads((root / 'Cargo.toml').read_text())
assert workspace['workspace']['package']['version'] == version, 'workspace/control mismatch'
meta = json.loads((root / 'META.json').read_text())
assert meta['version'] == meta['provides']['pgokf']['version'] == version, 'PGXN/control mismatch'
lock = tomllib.loads((root / 'Cargo.lock').read_text())
for path in root.glob('crates/*/Cargo.toml'):
    manifest = tomllib.loads(path.read_text())
    package = manifest['package']
    assert package['version'] == {'workspace': True} or package['version'] == version, path
    entries = [p for p in lock['package'] if p['name'] == package['name']]
    assert len(entries) == 1 and entries[0]['version'] == version, path
    for section in ('dependencies', 'dev-dependencies', 'build-dependencies'):
        for dep in manifest.get(section, {}).values():
            if isinstance(dep, dict) and 'path' in dep:
                assert dep.get('version') == '=' + version, path
IDENTITY

publish=false
source_ref=$(git rev-parse HEAD)
head=$(git rev-parse HEAD)
if [[ "$GITHUB_REF" == refs/tags/v* ]]; then
    tag="${GITHUB_REF#refs/tags/v}"
    if [[ "$tag" != "$version" ]]; then
        echo "::error::tag v$tag does not match pgokf.control default_version $version"
        exit 1
    fi
    tagged=$(git rev-parse "$GITHUB_REF^{}")
    if [[ "$tagged" != "$head" ]]; then
        echo "::error::HEAD $head is not the commit of tag $GITHUB_REF ($tagged)"
        exit 1
    fi
    publish=true
    source_ref="$head"
elif [[ -n "${RELEASE_TAG_INPUT:-}" ]]; then
    # Manual catch-up: republish from an explicit existing immutable tag.
    case "$RELEASE_TAG_INPUT" in
        v*) ;;
        *) echo "::error::release_tag must name a version tag (got '$RELEASE_TAG_INPUT')"; exit 1 ;;
    esac
    tag="${RELEASE_TAG_INPUT#v}"
    if [[ "$tag" != "$version" ]]; then
        echo "::error::release_tag $RELEASE_TAG_INPUT does not match pgokf.control default_version $version at that tag"
        exit 1
    fi
    tagged=$(git rev-parse "refs/tags/$RELEASE_TAG_INPUT^{}")
    if [[ "$tagged" != "$head" ]]; then
        echo "::error::checked-out HEAD $head is not the commit of $RELEASE_TAG_INPUT ($tagged); refusing to publish branch source under a release name"
        exit 1
    fi
    publish=true
    source_ref="$head"
fi
echo "publish=$publish" >> "$GITHUB_OUTPUT"
echo "source_ref=$source_ref" >> "$GITHUB_OUTPUT"
echo "publish images: $publish (source ref: ${source_ref:-<event commit>})"
