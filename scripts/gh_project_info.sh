#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"
require_yq

require_gh() {
  if ! command -v gh >/dev/null 2>&1; then
    echo "gh is required but not found in PATH." >&2
    exit 1
  fi
}

require_gh
init_config_file

if [ "${1:-}" = "--fix" ]; then
  "$(dirname "$0")/gh_project_apply.sh"
  exit 0
fi

PROJECT_ID=${GITHUB_PROJECT_ID:-$(config_get '.gh.project_id // ""')}
if [ -z "$PROJECT_ID" ] || [ "$PROJECT_ID" = "null" ]; then
  echo "Missing gh.project_id. Set it in config.yml or export GITHUB_PROJECT_ID." >&2
  exit 1
fi

FIELDS_JSON=$(gh api graphql -f query='query($project:ID!){ node(id:$project){ ... on ProjectV2 { title fields(first:100){ nodes{ ... on ProjectV2SingleSelectField { id name dataType options { id name } } ... on ProjectV2Field { id name dataType } } } } } }' -f project="$PROJECT_ID")

echo "Project:"
printf '%s' "$FIELDS_JSON" | yq -r '.data.node.title'
echo
echo "Fields:"
printf '%s' "$FIELDS_JSON" | yq -r '.data.node.fields.nodes[] | [.name, .id, (.dataType // "")] | @tsv' | column -t -s $'\t'
echo
echo "Status field:"
printf '%s' "$FIELDS_JSON" | yq -r '.data.node.fields.nodes[] | select(.dataType == "SINGLE_SELECT" and .name == "Status") | [.name, .id, .dataType] | @tsv' | column -t -s $'\t'
echo
echo "Status options:"
printf '%s' "$FIELDS_JSON" | yq -r '.data.node.fields.nodes[] | select(.dataType == "SINGLE_SELECT" and .name == "Status") | .options[] | [.name, .id] | @tsv' | column -t -s $'\t'
