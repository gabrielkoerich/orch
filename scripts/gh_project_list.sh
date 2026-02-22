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

ORG=${1:-}
USER=${2:-}

list_managed_bare_projects() {
  local projects_dir="${ORCH_HOME:-$HOME/.orchestrator}/projects"
  if [ ! -d "$projects_dir" ]; then
    echo "No managed bare-clone projects found (missing: $projects_dir)"
    return 0
  fi

  local found=0
  local rows=""
  local bare slug
  while IFS= read -r bare; do
    [ -n "$bare" ] || continue
    slug=$(git -C "$bare" config remote.origin.url 2>/dev/null \
      | sed -E 's#^https?://github\.com/##; s#^git@github\.com:##; s#\.git$##' || true)
    [ -n "$slug" ] || slug="$(basename "$(dirname "$bare")")/$(basename "$bare" .git)"
    rows+="${slug}"$'\t'"${bare}"$'\n'
    found=1
  done < <(find "$projects_dir" -name "*.git" -type d -mindepth 2 -maxdepth 2 2>/dev/null | sort)

  if [ "$found" -eq 0 ]; then
    echo "No managed bare-clone projects found in $projects_dir"
    return 0
  fi

  {
    printf 'REPO\tPATH\n'
    printf '%s' "$rows"
  } | column -t -s $'\t'
}

if [ -n "$ORG" ]; then
  gh api graphql -f query='query($org:String!){ organization(login:$org){ projectsV2(first:100){ nodes{ id number title } } } }' -f org="$ORG" \
    | yq -r '.data.organization.projectsV2.nodes[] | [.number, .title, .id] | @tsv' | column -t -s $'\t'
  echo ""
  echo "=== Managed projects (bare clones) ==="
  list_managed_bare_projects
  exit 0
fi

if [ -n "$USER" ]; then
  gh api graphql -f query='query($user:String!){ user(login:$user){ projectsV2(first:100){ nodes{ id number title } } } }' -f user="$USER" \
    | yq -r '.data.user.projectsV2.nodes[] | [.number, .title, .id] | @tsv' | column -t -s $'\t'
  echo ""
  echo "=== Managed projects (bare clones) ==="
  list_managed_bare_projects
  exit 0
fi

list_managed_bare_projects
