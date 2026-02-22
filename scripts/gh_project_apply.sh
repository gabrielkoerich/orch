#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"
require_yq
PROJECT_DIR="${PROJECT_DIR:-$(pwd)}"
export PROJECT_DIR

require_gh() {
  if ! command -v gh >/dev/null 2>&1; then
    echo "gh is required but not found in PATH." >&2
    exit 1
  fi
}

require_gh
init_config_file
load_project_config

PROJECT_ID=${GITHUB_PROJECT_ID:-$(config_get '.gh.project_id // ""')}
if [ -z "$PROJECT_ID" ] || [ "$PROJECT_ID" = "null" ]; then
  echo "Missing gh.project_id. Set it in config.yml or export GITHUB_PROJECT_ID." >&2
  exit 1
fi

FIELDS_JSON=$(gh api graphql -f query='query($project:ID!){ node(id:$project){ ... on ProjectV2 { fields(first:100){ nodes{ ... on ProjectV2SingleSelectField { id name options { id name } } } } } } }' -f project="$PROJECT_ID")

STATUS_FIELD_ID=$(printf '%s' "$FIELDS_JSON" | yq -r '.data.node.fields.nodes[] | select(.name == "Status") | .id' | head -n1)

if [ -z "$STATUS_FIELD_ID" ] || [ "$STATUS_FIELD_ID" = "null" ]; then
  echo "Status field not found in project." >&2
  exit 1
fi

# Helper: find option ID matching any of the given names (case-insensitive)
read_status_names() {
  local key="$1"
  if command -v jq >/dev/null 2>&1; then
    yq -o=json ".gh.project_status_names.${key} // null" "$CONFIG_PATH" 2>/dev/null | \
      jq -r 'if . == null then empty elif type == "array" then .[] elif type == "string" then . else empty end' 2>/dev/null
    return 0
  fi

  local val
  val=$(yq -r ".gh.project_status_names.${key} // \"\"" "$CONFIG_PATH" 2>/dev/null || true)
  [ -n "$val" ] && [ "$val" != "null" ] && printf '%s\n' "$val"
}

find_option_id() {
  local json="$1"; shift
  for name in "$@"; do
    [ -n "$name" ] || continue
    local id
    local name_lower
    name_lower=$(printf '%s' "$name" | tr '[:upper:]' '[:lower:]')
    id=$(printf '%s' "$json" | NAME_LOWER="$name_lower" yq -r '.data.node.fields.nodes[] | select(.name == "Status") | .options[] | select(.name | downcase == strenv(NAME_LOWER)) | .id' 2>/dev/null | head -n1)
    if [ -n "$id" ] && [ "$id" != "null" ]; then
      echo "$id"
      return
    fi
  done
}

mapfile -t backlog_names < <(read_status_names backlog)
mapfile -t inprog_names < <(read_status_names in_progress)
mapfile -t review_names < <(read_status_names review)
mapfile -t done_names < <(read_status_names done)

if [ ${#backlog_names[@]} -eq 0 ]; then backlog_names=("Backlog"); fi
if [ ${#inprog_names[@]} -eq 0 ]; then inprog_names=("In Progress"); fi
if [ ${#review_names[@]} -eq 0 ]; then review_names=("Review"); fi
if [ ${#done_names[@]} -eq 0 ]; then done_names=("Done"); fi

BACKLOG_ID=$(find_option_id "$FIELDS_JSON" "${backlog_names[@]}")
INPROG_ID=$(find_option_id "$FIELDS_JSON" "${inprog_names[@]}")
REVIEW_ID=$(find_option_id "$FIELDS_JSON" "${review_names[@]}")
DONE_ID=$(find_option_id "$FIELDS_JSON" "${done_names[@]}")

export STATUS_FIELD_ID
yq -i ".gh.project_status_field_id = strenv(STATUS_FIELD_ID)" "$GLOBAL_CONFIG_PATH"

export BACKLOG_ID INPROG_ID REVIEW_ID DONE_ID
[ -n "$BACKLOG_ID" ] && [ "$BACKLOG_ID" != "null" ] && yq -i '.gh.project_status_map.backlog = strenv(BACKLOG_ID)' "$GLOBAL_CONFIG_PATH"
[ -n "$INPROG_ID" ] && [ "$INPROG_ID" != "null" ] && yq -i '.gh.project_status_map.in_progress = strenv(INPROG_ID)' "$GLOBAL_CONFIG_PATH"
[ -n "$REVIEW_ID" ] && [ "$REVIEW_ID" != "null" ] && yq -i '.gh.project_status_map.review = strenv(REVIEW_ID)' "$GLOBAL_CONFIG_PATH"
[ -n "$DONE_ID" ] && [ "$DONE_ID" != "null" ] && yq -i '.gh.project_status_map.done = strenv(DONE_ID)' "$GLOBAL_CONFIG_PATH"

echo "Applied Status field and option IDs to config.yml"
