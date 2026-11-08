#!/usr/bin/env bash
set -euo pipefail

: "${GITHUB_EVENT_NAME:?GitHub event name is required}"
: "${GITHUB_OUTPUT:?GitHub output file is required}"

version="$({
  jq -er '
    if (.workspace_members | type) != "array" or (.packages | type) != "array" then
      error("Cargo metadata packages and workspace_members must be arrays")
    elif any(.workspace_members[]; type != "string") then
      error("Cargo metadata workspace member IDs must be strings")
    elif any(.packages[]; type != "object" or (.id | type) != "string" or (.name | type) != "string" or (.version | type) != "string") then
      error("Cargo metadata packages require string id, name, and version fields")
    elif ([.packages[].id] | unique | length) != (.packages | length) then
      error("Cargo metadata package IDs must be unique")
    else
      .workspace_members as $members
      | .packages as $packages
      | [$packages[] | select(.id as $id | $members | index($id))] as $workspace
      | [$workspace[] | select(.name == "pgokf")] as $pgokf
      | if ($members | length) == 0 or ($workspace | length) != ($members | length) then
          error("every workspace member must resolve to one package")
        elif ($members | unique | length) != ($members | length) then
          error("workspace member IDs must be unique")
        elif ($pgokf | length) != 1 then
          error("exactly one pgokf workspace package is required")
        elif all($workspace[]; .version == $pgokf[0].version) then
          $pgokf[0].version
        else
          error("workspace package versions do not match pgokf")
        end
      | select(test("^(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)(-(0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)(\\.(0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*)?(\\+[0-9A-Za-z-]+(\\.[0-9A-Za-z-]+)*)?\\z"))
    end
  '
})"

deploy=false
case "$GITHUB_EVENT_NAME" in
  push)
    # Every main push builds strictly, but publication is always manual-only.
    deploy=false
    ;;
  workflow_dispatch)
    case "${INPUT_DEPLOY_PAGES:-false}" in
      true) deploy=true ;;
      false | "") deploy=false ;;
      *)
        echo "deploy_pages must be true or false" >&2
        exit 1
        ;;
    esac
    ;;
  *)
    echo "unsupported Pages event: $GITHUB_EVENT_NAME" >&2
    exit 1
    ;;
esac

{
  printf 'version=%s\n' "$version"
  printf 'deploy=%s\n' "$deploy"
} >>"$GITHUB_OUTPUT"
